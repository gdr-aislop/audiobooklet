//! Multi-server/account management: adding a server and logging in, switching the active
//! account, and signing out. Thin orchestration over `abs-storage::repo::{servers, accounts}`
//! and `abs_api::Client::login` — the interesting invariants (exactly one active account,
//! cascade cleanup) already live in the storage layer's tests; this module's tests focus on the
//! login-then-persist flow and its failure modes.

use abs_storage::repo::{accounts, servers};
use abs_storage::AppPaths;
use sqlx::SqlitePool;

use crate::error::Result;
use crate::passwords::PasswordStore;

pub struct AddedAccount {
    pub server_id: String,
    pub account_id: String,
}

/// How to sign in: a username and password, or an API key (a bearer token created in the
/// server's settings, with no username and no refresh token).
pub enum SignIn<'a> {
    Password { username: &'a str, password: &'a str },
    ApiKey(&'a str),
}

/// A client certificate bundle (PKCS#12) and its export password.
#[derive(Clone, PartialEq)]
pub struct ClientCertificate {
    pub path: std::path::PathBuf,
    pub password: Option<String>,
}

impl std::fmt::Debug for ClientCertificate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientCertificate").field("path", &self.path).field("password", &self.password.as_ref().map(|_| "<redacted>")).finish()
    }
}

/// Connection options chosen on the login screen's Advanced section, for a server that may not
/// have a row yet. They apply to the attempt on top of what a known server's row already holds,
/// and are saved on the server's row once the sign-in succeeds (a failed attempt saves nothing).
/// Empty by default: nothing chosen, the row's own settings apply.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConnectionSettings {
    /// Trust any certificate (the server row's "Disable SSL verification").
    pub allow_untrusted_certificate: bool,
    /// The canonical JSON `abs_core::connection::validate_custom_headers_text` produces.
    pub custom_headers_json: Option<String>,
    pub client_certificate: Option<ClientCertificate>,
}

/// The options for an attempt: `base` (a known server's saved options, or none) with whatever
/// `settings` provides laid over it.
fn attempt_options(base: Option<abs_api::ConnectionOptions>, settings: &ConnectionSettings) -> abs_api::ConnectionOptions {
    let mut options = base.unwrap_or_default();
    if settings.allow_untrusted_certificate {
        options.disable_ssl_verify = true;
    }
    if let Some(json) = &settings.custom_headers_json {
        options.extra_headers = crate::connection::parse_custom_headers(json);
    }
    if let Some(certificate) = &settings.client_certificate {
        options.client_cert_path = Some(certificate.path.clone());
        options.client_cert_password = certificate.password.clone().filter(|password| !password.is_empty());
    }
    options
}

/// Writes the settings the user provided onto the server's row, after a successful sign-in.
async fn save_settings(pool: &SqlitePool, server_id: &str, settings: &ConnectionSettings) -> Result<()> {
    if settings.allow_untrusted_certificate {
        servers::set_disable_ssl_verify(pool, server_id, true).await?;
    }
    if let Some(json) = &settings.custom_headers_json {
        servers::set_custom_headers_json(pool, server_id, json).await?;
    }
    if let Some(certificate) = &settings.client_certificate {
        servers::set_client_cert_path(pool, server_id, Some(&certificate.path.to_string_lossy())).await?;
        servers::set_client_cert_password(pool, server_id, certificate.password.as_deref().filter(|password| !password.is_empty())).await?;
    }
    Ok(())
}

/// A connection setting that can't be used, in words a person can act on.
fn setup_error(err: abs_api::ConnectionBuildError) -> crate::error::CoreError {
    use abs_api::ConnectionBuildError::*;
    crate::error::CoreError::ConnectionSetup(match err {
        InvalidBearerToken => "The API key contains characters that can't be used.".to_string(),
        InvalidHeader(name) => format!("The custom header {name:?} isn't valid."),
        ClientCertificate { path, .. } => {
            let file = path.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string_lossy().to_string());
            format!("The client certificate {file} couldn't be opened. Check that the file is still there and that its password is right.")
        }
        other => other.to_string(),
    })
}

/// Signs in to the server at `url` with `options`, returning what the account row stores. An
/// API key is a bearer token: `/api/me` says whose it is, and there is nothing to refresh.
async fn authenticate(url: &str, options: &abs_api::ConnectionOptions, sign_in: &SignIn<'_>) -> Result<abs_api::LoginResult> {
    match sign_in {
        SignIn::Password { username, password } => {
            let client = abs_api::Client::with_options(url, options).map_err(setup_error)?;
            Ok(client.login(username, password).await?)
        }
        SignIn::ApiKey(key) => {
            let key = key.trim();
            let client = abs_api::Client::with_bearer_token_and_options(url, key, std::time::Duration::from_secs(15), options).map_err(setup_error)?;
            let who = client.whoami().await?;
            Ok(abs_api::LoginResult { user_id: who.user_id, username: who.username, access_token: key.to_string(), refresh_token: None })
        }
    }
}

/// Remembers (or forgets) a password after a sign-in; an API key has none to remember.
async fn remember_after_sign_in(passwords: &dyn PasswordStore, account_id: &str, sign_in: &SignIn<'_>, remember: bool) {
    if let SignIn::Password { password, .. } = sign_in {
        crate::passwords::remember_after_login(passwords, account_id, password, remember).await;
    }
}

/// The previous session's identity, captured by the shell before sending the user back to the
/// login screen after an authorization failure. `relogin` compares what the user typed against
/// this to decide whether a successful login is a token rotation on the same account, a different
/// account on the same server, or a completely different server — each with different cleanup.
#[derive(Clone)]
pub struct ReloginSeed {
    pub server_id: String,
    pub account_id: String,
    pub url: String,
    pub username: String,
}

/// What a prospective re-login would do to the current session's local data. `None` means the
/// same URL and username as the seeded session — signing in again just rotates tokens and
/// touches nothing else.
pub enum ReplacementKind {
    /// Same server URL, different username: the server's cached libraries/items/covers/downloads
    /// stay valid (they're keyed by `server_id`), but the old account's progress and bookmarks
    /// cascade away with its row.
    SameServerNewAccount,
    /// Different server URL: everything cached for the old server — rows via FK cascade, and the
    /// on-disk cover/download files — is removed.
    NewServer,
}

/// URLs equal after trimming and dropping trailing slashes — the tolerance a user typing (or
/// pasting) the same address slightly differently should get. Scheme or host spelling differences
/// are genuinely different addresses and compare unequal.
fn normalize_url(url: &str) -> &str {
    servers::normalize_url(url)
}

/// Classifies a prospective re-login against the seeded session. Pure so the Welcome screen can
/// drive its confirmation dialog and `relogin` can drive the actual cleanup from one source of
/// truth — the dialog is only ever a preview of what this function says will happen.
pub fn replacement_kind(seed: &ReloginSeed, url: &str, username: &str) -> Option<ReplacementKind> {
    if normalize_url(url) == normalize_url(&seed.url) {
        if username == seed.username {
            None
        } else {
            Some(ReplacementKind::SameServerNewAccount)
        }
    } else {
        Some(ReplacementKind::NewServer)
    }
}

/// Log into a server from the Welcome/Add Server screen and make that account the active one —
/// the only way to add an account, so leaving it inactive would mean the app forgets you're signed
/// in the next time it starts. With `remember`, the password is kept in `passwords` so the session
/// can sign in again on its own later.
///
/// A server already in the list is reused rather than added again (found in the 2026-10-12 pass:
/// the same address could be added any number of times, each copy with its own cache and
/// downloads):
/// - an existing account for the same user on that server just gets the new tokens and becomes
///   active, keeping its progress and downloads;
/// - a server row left without an account (after signing out of it) gets the new account, and
///   keeps its cached libraries and downloads (the next sync drops whatever this user can't see);
/// - a different user on a server that already has an account gets a row of its own, because
///   users may see different libraries and one cache can't hold both views.
///
/// The URL is stored normalized (see [`servers::normalize_url`]). A failed login stores nothing.
pub async fn add_server_and_login(
    pool: &SqlitePool,
    url: &str,
    username: &str,
    password: &str,
    passwords: &dyn PasswordStore,
    remember: bool,
) -> Result<AddedAccount> {
    add_server_and_sign_in(pool, url, &SignIn::Password { username, password }, passwords, remember, &ConnectionSettings::default()).await
}

/// [`add_server_and_login`] for either kind of credential, with connection options chosen on the
/// login screen. A known server's saved options apply to the attempt, with `settings` laid over
/// them; on success the options `settings` provides are saved on the server's row.
pub async fn add_server_and_sign_in(
    pool: &SqlitePool,
    url: &str,
    sign_in: &SignIn<'_>,
    passwords: &dyn PasswordStore,
    remember: bool,
    settings: &ConnectionSettings,
) -> Result<AddedAccount> {
    let url = servers::normalize_url(url);
    let known = servers::find_by_url(pool, url).await?;

    // A known server's saved connection settings (custom headers, TLS) apply to this login too.
    let options = attempt_options(known.first().map(|row| crate::connection::ConnectionTarget::resolve(row, None).options), settings);
    let login_result = authenticate(url, &options, sign_in).await?;

    let mut empty_row = None;
    for server in &known {
        let accounts_here = accounts::list_for_server(pool, &server.id).await?;
        if let Some(account) = accounts_here.iter().find(|a| a.username == login_result.username) {
            accounts::set_tokens(pool, &account.id, &login_result.access_token, login_result.refresh_token.as_deref()).await?;
            accounts::set_active(pool, &account.id).await?;
            save_settings(pool, &server.id, settings).await?;
            remember_after_sign_in(passwords, &account.id, sign_in, remember).await;
            tracing::info!(account_id = %account.id, "signed in again to an account already on this device");
            return Ok(AddedAccount { server_id: server.id.clone(), account_id: account.id.clone() });
        }
        if accounts_here.is_empty() && empty_row.is_none() {
            empty_row = Some(server.id.clone());
        }
    }

    let (server_id, new_row) = match empty_row {
        Some(server_id) => (server_id, false),
        None => (servers::add(pool, url).await?, true),
    };
    let account_id = match accounts::add(
        pool,
        &server_id,
        &login_result.username,
        &login_result.access_token,
        login_result.refresh_token.as_deref(),
    )
    .await
    {
        Ok(account_id) => account_id,
        Err(err) => {
            // Don't leave a credential-less server behind.
            if new_row {
                let _ = servers::remove(pool, &server_id).await;
            }
            return Err(err.into());
        }
    };
    accounts::set_active(pool, &account_id).await?;
    save_settings(pool, &server_id, settings).await?;
    remember_after_sign_in(passwords, &account_id, sign_in, remember).await;
    if !new_row {
        tracing::info!(%server_id, "signed in to a server already on this device");
    }

    Ok(AddedAccount { server_id, account_id })
}

/// Makes sure an account is active whenever any is left: when none is (the active one was
/// signed out or its server removed), the most recently used remaining account becomes active.
/// Returns the active account's id, or `None` when no account is left. Found in the 2026-10-12
/// pass: signing out of the active account used to leave the app on the first-run login screen,
/// with no way back to the other accounts.
pub async fn ensure_active(pool: &SqlitePool) -> Result<Option<String>> {
    if let Some(active) = accounts::get_active(pool).await? {
        return Ok(Some(active.id));
    }
    let Some(next) = accounts::most_recent(pool).await? else { return Ok(None) };
    accounts::set_active(pool, &next.id).await?;
    tracing::info!(account_id = %next.id, "no account was active; switched to the one used most recently");
    Ok(Some(next.id))
}

/// Sign in again after the previous session died (revoked/expired refresh token, removed user) or
/// the user wants a different account/server — the flow behind the "Log in again" button on
/// Home's failure state. The caller (the Welcome screen) has already confirmed any replacement
/// with the user via `replacement_kind`; this function then guarantees the invariants:
///
/// - a failed login persists nothing — the old session stays active and reachable;
/// - same URL + username (server-canonical) → only the token pair is rotated, all local data is
///   kept, ids come back unchanged;
/// - same URL, different username → the old account row is removed (its progress/bookmarks
///   cascade) but the server's cache survives, since it's keyed by `server_id`;
/// - different URL → the old server row is removed entirely (cascading its libraries, items,
///   chapters and downloads) and its on-disk cover/download files are purged.
///
/// The signed-in account's password is then remembered in `passwords` or forgotten, per
/// `remember`; a removed previous account's is forgotten.
#[allow(clippy::too_many_arguments)]
pub async fn relogin(
    pool: &SqlitePool,
    paths: &AppPaths,
    previous: &ReloginSeed,
    url: &str,
    username: &str,
    password: &str,
    passwords: &dyn PasswordStore,
    remember: bool,
) -> Result<AddedAccount> {
    relogin_with(pool, paths, previous, url, &SignIn::Password { username, password }, passwords, remember, &ConnectionSettings::default()).await
}

/// The options a re-login attempt uses: the previous server's saved ones when `url` is that same
/// server (see [`relogin_with`]), with `settings` laid over them.
async fn relogin_options(pool: &SqlitePool, previous: &ReloginSeed, url: &str, settings: &ConnectionSettings) -> abs_api::ConnectionOptions {
    let saved = match servers::get(pool, &previous.server_id).await {
        Ok(row) if normalize_url(&row.url) == url => Some(crate::connection::ConnectionTarget::resolve(&row, None).options),
        _ => None,
    };
    attempt_options(saved, settings)
}

/// Whose API key this is, asked of the server the way [`relogin_with`] would sign in — so the
/// re-login screen can run [`replacement_kind`] (and confirm a replacement) before anything
/// changes. A key carries no username to compare until the server has been asked.
pub async fn api_key_username(pool: &SqlitePool, previous: &ReloginSeed, url: &str, key: &str, settings: &ConnectionSettings) -> Result<String> {
    let url = normalize_url(url);
    let options = relogin_options(pool, previous, url, settings).await;
    Ok(authenticate(url, &options, &SignIn::ApiKey(key)).await?.username)
}

/// [`relogin`] for either kind of credential, with connection options chosen on the login
/// screen (see [`add_server_and_sign_in`]).
#[allow(clippy::too_many_arguments)]
pub async fn relogin_with(
    pool: &SqlitePool,
    paths: &AppPaths,
    previous: &ReloginSeed,
    url: &str,
    sign_in: &SignIn<'_>,
    passwords: &dyn PasswordStore,
    remember: bool,
    settings: &ConnectionSettings,
) -> Result<AddedAccount> {
    // Normalized once, up front: every use below (the HTTP client's baseurl, the same-server
    // comparison, the new server row) sees the same canonical form, so a user-typed trailing
    // slash can't fork the flow.
    let url = normalize_url(url);
    // A same-server re-login honors the server's saved connection settings — a self-signed
    // server must be reachable through its own TLS policy, or "sign in again" would be the one
    // flow that breaks exactly where every other call works. Any other case (a different
    // server) has no saved settings to honor: defaults apply, plus whatever the login screen's
    // Advanced section provides.
    let options = relogin_options(pool, previous, url, settings).await;
    let login_result = authenticate(url, &options, sign_in).await?;

    let same_server = url == normalize_url(&previous.url);
    let same_account = same_server && login_result.username == previous.username;

    if same_account {
        accounts::set_tokens(
            pool,
            &previous.account_id,
            &login_result.access_token,
            login_result.refresh_token.as_deref(),
        )
        .await?;
        save_settings(pool, &previous.server_id, settings).await?;
        remember_after_sign_in(passwords, &previous.account_id, sign_in, remember).await;
        return Ok(AddedAccount {
            server_id: previous.server_id.clone(),
            account_id: previous.account_id.clone(),
        });
    }

    if same_server {
        let account_id = accounts::add(
            pool,
            &previous.server_id,
            &login_result.username,
            &login_result.access_token,
            login_result.refresh_token.as_deref(),
        )
        .await?;
        accounts::set_active(pool, &account_id).await?;
        accounts::remove(pool, &previous.account_id).await?;
        crate::passwords::forget(passwords, &previous.account_id).await;
        save_settings(pool, &previous.server_id, settings).await?;
        remember_after_sign_in(passwords, &account_id, sign_in, remember).await;
        return Ok(AddedAccount { server_id: previous.server_id.clone(), account_id });
    }

    let server_id = servers::add(pool, url).await?;
    let account_id = match accounts::add(
        pool,
        &server_id,
        &login_result.username,
        &login_result.access_token,
        login_result.refresh_token.as_deref(),
    )
    .await
    {
        Ok(account_id) => account_id,
        Err(err) => {
            // Same rollback discipline as `add_server_and_login`: no credential-less server row.
            let _ = servers::remove(pool, &server_id).await;
            return Err(err.into());
        }
    };
    accounts::set_active(pool, &account_id).await?;
    servers::remove(pool, &previous.server_id).await?;
    crate::passwords::forget(passwords, &previous.account_id).await;
    save_settings(pool, &server_id, settings).await?;
    remember_after_sign_in(passwords, &account_id, sign_in, remember).await;
    if let Err(err) = paths.purge_server_data(&previous.server_id).await {
        tracing::warn!(%err, server_id = %previous.server_id, "couldn't purge the old server's on-disk cache after switching servers; the files are orphaned but harmless");
    }

    Ok(AddedAccount { server_id, account_id })
}

/// Switch the active account, which may belong to a different server than whatever was
/// previously active — this is the "Switch to this server" action from Settings' per-server menu.
pub async fn switch_active_account(pool: &SqlitePool, account_id: &str) -> Result<()> {
    accounts::set_active(pool, account_id).await?;
    Ok(())
}

/// Sign out of one account. Its local progress rows cascade-delete with it (see the storage
/// layer's migration); the server itself and any other accounts on it are untouched. Its
/// remembered password is forgotten. If it was the active account, the most recently used
/// remaining account becomes active ([`ensure_active`]).
pub async fn sign_out(pool: &SqlitePool, account_id: &str, passwords: &dyn PasswordStore) -> Result<()> {
    accounts::remove(pool, account_id).await?;
    crate::passwords::forget(passwords, account_id).await;
    ensure_active(pool).await?;
    Ok(())
}

/// Remove a server entirely — "Remove Server" from Settings' per-server menu. Cascades to every
/// account, library, item, and download record for that server, and forgets those accounts'
/// remembered passwords. If it held the active account, the most recently used remaining account
/// becomes active ([`ensure_active`]).
pub async fn remove_server(pool: &SqlitePool, server_id: &str, passwords: &dyn PasswordStore) -> Result<()> {
    let removed = accounts::list_for_server(pool, server_id).await?;
    servers::remove(pool, server_id).await?;
    for account in removed {
        crate::passwords::forget(passwords, &account.id).await;
    }
    ensure_active(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CoreError;
    use crate::passwords::{MemoryPasswords, NoPasswords};
    use abs_storage::connect_and_migrate;
    use abs_storage::repo::items::{self, UpsertItem};
    use abs_storage::repo::libraries::{self, UpsertLibrary};
    use abs_storage::repo::progress;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn pool() -> SqlitePool {
        let tmp = tempfile::tempdir().unwrap();
        let pool = connect_and_migrate(&tmp.path().join("db.sqlite3")).await.unwrap();
        std::mem::forget(tmp);
        pool
    }

    fn mock_login_success() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "user": {
                "id": "user-1",
                "username": "jane",
                "accessToken": "abc123",
                "refreshToken": "refresh456",
            }
        }))
    }

    /// One synced library + one item + one progress row, so the tests can observe what a
    /// replacement keeps and what it throws away.
    async fn seed_cache(pool: &SqlitePool, server_id: &str, account_id: &str) {
        libraries::upsert(
            pool,
            UpsertLibrary {
                id: "lib-1",
                server_id,
                name: "Audiobooks",
                media_type: "book",
                icon: None,
                display_order: 1,
            },
        )
        .await
        .unwrap();
        items::upsert(
            pool,
            UpsertItem {
                id: "item-1",
                server_id,
                library_id: "lib-1",
                title: "Project Hail Mary",
                author: Some("Andy Weir"),
                narrator: None,
                description: None,
                duration_seconds: 11600.0,
                added_at: chrono::Utc::now(),
                series_name: None,
                genres: &[],
            },
        )
        .await
        .unwrap();
        progress::set(pool, account_id, server_id, "item-1", 42.0, false).await.unwrap();
    }

    #[tokio::test]
    async fn add_server_and_login_persists_server_and_account() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/login")).respond_with(mock_login_success()).mount(&server).await;

        let pool = pool().await;
        let added = add_server_and_login(&pool, &server.uri(), "jane", "hunter2", &NoPasswords, false)
            .await
            .unwrap();

        let stored_server = servers::get(&pool, &added.server_id).await.unwrap();
        assert_eq!(stored_server.url, server.uri());

        let stored_account = accounts::get(&pool, &added.account_id).await.unwrap();
        assert_eq!(stored_account.username, "jane");
        assert_eq!(stored_account.token, "abc123");
        assert_eq!(stored_account.refresh_token.as_deref(), Some("refresh456"), "the refresh token must be persisted — it's what keeps the session alive past the access token's expiry");
        assert!(stored_account.is_active, "the newly added account should become active");
    }

    #[tokio::test]
    async fn failed_login_does_not_leave_an_orphaned_server_row() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/login")).respond_with(ResponseTemplate::new(401)).mount(&server).await;

        let pool = pool().await;
        let result = add_server_and_login(&pool, &server.uri(), "jane", "wrong", &NoPasswords, false).await;

        assert!(matches!(result, Err(CoreError::Login(_))));
        assert!(servers::list(&pool).await.unwrap().is_empty(), "no server row should remain");
    }

    #[tokio::test]
    async fn switch_active_account_delegates_to_storage() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/login")).respond_with(mock_login_success()).mount(&server).await;

        let pool = pool().await;
        let added = add_server_and_login(&pool, &server.uri(), "jane", "hunter2", &NoPasswords, false).await.unwrap();

        switch_active_account(&pool, &added.account_id).await.unwrap();
        assert!(accounts::get(&pool, &added.account_id).await.unwrap().is_active);
    }

    #[tokio::test]
    async fn sign_out_removes_the_account_but_not_the_server() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/login")).respond_with(mock_login_success()).mount(&server).await;

        let pool = pool().await;
        let added = add_server_and_login(&pool, &server.uri(), "jane", "hunter2", &NoPasswords, false).await.unwrap();

        sign_out(&pool, &added.account_id, &NoPasswords).await.unwrap();

        assert!(accounts::get(&pool, &added.account_id).await.is_err());
        assert!(servers::get(&pool, &added.server_id).await.is_ok(), "server should survive");
    }

    #[tokio::test]
    async fn remove_server_cascades_to_its_accounts() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/login")).respond_with(mock_login_success()).mount(&server).await;

        let pool = pool().await;
        let added = add_server_and_login(&pool, &server.uri(), "jane", "hunter2", &NoPasswords, false).await.unwrap();

        remove_server(&pool, &added.server_id, &NoPasswords).await.unwrap();

        assert!(servers::get(&pool, &added.server_id).await.is_err());
        assert!(accounts::get(&pool, &added.account_id).await.is_err());
    }

    /// A login mock that answers with whatever username was sent, like a real server.
    async fn mount_echo_login(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/login"))
            .respond_with(|req: &wiremock::Request| {
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
                let username = body["username"].as_str().unwrap().to_string();
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "user": { "id": format!("id-{username}"), "username": username, "accessToken": format!("access-{username}-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()), "refreshToken": "refresh" }
                }))
            })
            .mount(server)
            .await;
    }

    /// Found in the 2026-10-12 pass: logging in again to a server already on the device made a
    /// second copy of it, with its own (empty) cache and downloads.
    #[tokio::test]
    async fn logging_in_again_as_the_same_user_reuses_the_server_and_account() {
        let server = MockServer::start().await;
        mount_echo_login(&server).await;
        let pool = pool().await;

        let first = add_server_and_login(&pool, &server.uri(), "jane", "pw", &NoPasswords, false).await.unwrap();
        seed_cache(&pool, &first.server_id, &first.account_id).await;
        let token_before = accounts::get(&pool, &first.account_id).await.unwrap().token;
        let again = add_server_and_login(&pool, &format!(" {}/ ", server.uri()), "jane", "pw", &NoPasswords, false).await.unwrap();

        assert_eq!(again.server_id, first.server_id);
        assert_eq!(again.account_id, first.account_id);
        assert_eq!(servers::list(&pool).await.unwrap().len(), 1);
        let account = accounts::get(&pool, &first.account_id).await.unwrap();
        assert_ne!(account.token, token_before, "the new login's tokens are kept");
        assert!(account.is_active);
        assert!(progress::get(&pool, &first.account_id, &first.server_id, "item-1").await.unwrap().is_some(), "progress kept");
        assert_eq!(servers::list(&pool).await.unwrap()[0].url, server.uri(), "stored without the spaces and slash");
    }

    #[tokio::test]
    async fn a_server_left_without_an_account_is_reused_and_a_different_user_gets_a_row() {
        let server = MockServer::start().await;
        mount_echo_login(&server).await;
        let pool = pool().await;

        let bob = add_server_and_login(&pool, &server.uri(), "bob", "pw", &NoPasswords, false).await.unwrap();
        seed_cache(&pool, &bob.server_id, &bob.account_id).await;
        sign_out(&pool, &bob.account_id, &NoPasswords).await.unwrap();
        let bob_again = add_server_and_login(&pool, &server.uri(), "bob", "pw", &NoPasswords, false).await.unwrap();
        assert_eq!(bob_again.server_id, bob.server_id, "the empty row is reused");
        assert_eq!(libraries::list_for_server(&pool, &bob.server_id).await.unwrap().len(), 1, "with its cache");

        let anna = add_server_and_login(&pool, &server.uri(), "anna", "pw", &NoPasswords, false).await.unwrap();
        assert_ne!(anna.server_id, bob.server_id, "another user's view of the server gets its own cache");
        assert_eq!(servers::list(&pool).await.unwrap().len(), 2);
    }

    /// Found in the 2026-10-12 pass: signing out of the active account (or removing its server)
    /// left no account active, and the app on the first-run screen with no way back.
    #[tokio::test]
    async fn signing_out_of_the_active_account_switches_to_the_one_used_before() {
        let (a, b, c) = (MockServer::start().await, MockServer::start().await, MockServer::start().await);
        for server in [&a, &b, &c] {
            mount_echo_login(server).await;
        }
        let pool = pool().await;
        let on_a = add_server_and_login(&pool, &a.uri(), "root", "pw", &NoPasswords, false).await.unwrap();
        let on_b = add_server_and_login(&pool, &b.uri(), "bob", "pw", &NoPasswords, false).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let on_c = add_server_and_login(&pool, &c.uri(), "demo", "pw", &NoPasswords, false).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        switch_active_account(&pool, &on_a.account_id).await.unwrap();

        sign_out(&pool, &on_a.account_id, &NoPasswords).await.unwrap();
        assert_eq!(accounts::get_active(&pool).await.unwrap().unwrap().id, on_c.account_id, "the one used most recently");

        remove_server(&pool, &on_c.server_id, &NoPasswords).await.unwrap();
        assert_eq!(accounts::get_active(&pool).await.unwrap().unwrap().id, on_b.account_id);

        sign_out(&pool, &on_b.account_id, &NoPasswords).await.unwrap();
        assert!(accounts::get_active(&pool).await.unwrap().is_none(), "nothing left: the first-run screen");
    }

    #[tokio::test]
    async fn signing_out_of_an_inactive_account_leaves_the_active_one_alone() {
        let (a, b) = (MockServer::start().await, MockServer::start().await);
        mount_echo_login(&a).await;
        mount_echo_login(&b).await;
        let pool = pool().await;
        let on_a = add_server_and_login(&pool, &a.uri(), "root", "pw", &NoPasswords, false).await.unwrap();
        let on_b = add_server_and_login(&pool, &b.uri(), "bob", "pw", &NoPasswords, false).await.unwrap();

        sign_out(&pool, &on_a.account_id, &NoPasswords).await.unwrap();
        assert_eq!(accounts::get_active(&pool).await.unwrap().unwrap().id, on_b.account_id);
    }

    #[test]
    fn replacement_kind_classifies_the_three_cases() {
        let seed = ReloginSeed {
            server_id: "s".into(),
            account_id: "a".into(),
            url: "http://host/abs".into(),
            username: "jane".into(),
        };
        assert!(replacement_kind(&seed, "http://host/abs", "jane").is_none(), "same credentials need no replacement");
        assert!(
            replacement_kind(&seed, "http://host/abs/", "jane").is_none(),
            "a trailing slash is the same address, not a different server"
        );
        assert!(
            matches!(replacement_kind(&seed, "http://host/abs", "bob"), Some(ReplacementKind::SameServerNewAccount)),
            "a different username on the same server replaces the account, not the server"
        );
        assert!(
            matches!(replacement_kind(&seed, "http://elsewhere/abs", "jane"), Some(ReplacementKind::NewServer)),
            "a different URL is a different server"
        );
    }

    #[tokio::test]
    async fn relogin_with_the_same_credentials_rotates_tokens_and_keeps_everything() {
        let server = MockServer::start().await;
        // `up_to_n_times(1)` — wiremock matches first-mounted first, so without this the seeding
        // mock would also answer the re-login and the rotation below would never be observed.
        Mock::given(method("POST"))
            .and(path("/login"))
            .respond_with(mock_login_success())
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let pool = pool().await;
        let added = add_server_and_login(&pool, &server.uri(), "jane", "hunter2", &NoPasswords, false).await.unwrap();
        seed_cache(&pool, &added.server_id, &added.account_id).await;
        let previous = ReloginSeed {
            server_id: added.server_id.clone(),
            account_id: added.account_id.clone(),
            url: server.uri(),
            username: "jane".into(),
        };
        // Fresh tokens on the re-login (the old pair is what died), and a trailing slash in the
        // URL to prove normalization doesn't send this down the new-server path.
        Mock::given(method("POST")).and(path("/login")).respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({
                "user": { "id": "user-1", "username": "jane", "accessToken": "new-access", "refreshToken": "new-refresh" },
            }),
        )).mount(&server).await;

        let relogged = relogin(&pool, &AppPaths::rooted_at("/tmp/unused", "/tmp/unused", "/tmp/unused"), &previous, &format!("{}/", server.uri()), "jane", "hunter2", &NoPasswords, false)
            .await
            .unwrap();

        assert_eq!(relogged.server_id, added.server_id, "same server, so the row must be reused, not duplicated");
        assert_eq!(relogged.account_id, added.account_id, "same account, so the row must be reused, not duplicated");
        let stored = accounts::get(&pool, &added.account_id).await.unwrap();
        assert_eq!(stored.token, "new-access");
        assert_eq!(stored.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(servers::list(&pool).await.unwrap().len(), 1, "no duplicate server row");
        assert_eq!(accounts::list_for_server(&pool, &added.server_id).await.unwrap().len(), 1, "no duplicate account row");
        assert_eq!(
            libraries::list_for_server(&pool, &added.server_id).await.unwrap().len(),
            1,
            "the cached library must survive a same-account re-login untouched"
        );
    }

    #[tokio::test]
    async fn relogin_to_a_different_account_on_the_same_server_keeps_the_cache_but_replaces_the_account() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/login")).respond_with(mock_login_success()).up_to_n_times(1).mount(&server).await;

        let pool = pool().await;
        let added = add_server_and_login(&pool, &server.uri(), "jane", "hunter2", &NoPasswords, false).await.unwrap();
        seed_cache(&pool, &added.server_id, &added.account_id).await;
        let previous = ReloginSeed {
            server_id: added.server_id.clone(),
            account_id: added.account_id.clone(),
            url: server.uri(),
            username: "jane".into(),
        };
        Mock::given(method("POST")).and(path("/login")).respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({
                "user": { "id": "user-2", "username": "bob", "accessToken": "bob-token", "refreshToken": "bob-refresh" },
            }),
        )).mount(&server).await;

        let relogged = relogin(&pool, &AppPaths::rooted_at("/tmp/unused", "/tmp/unused", "/tmp/unused"), &previous, &server.uri(), "bob", "hunter2", &NoPasswords, false)
            .await
            .unwrap();

        assert_eq!(relogged.server_id, added.server_id, "the server row is reused — its cache is still valid");
        assert_ne!(relogged.account_id, added.account_id);
        assert!(accounts::get(&pool, &added.account_id).await.is_err(), "the old account must go");
        assert_eq!(accounts::get(&pool, &relogged.account_id).await.unwrap().username, "bob");
        assert!(accounts::get(&pool, &relogged.account_id).await.unwrap().is_active);
        assert_eq!(
            libraries::list_for_server(&pool, &added.server_id).await.unwrap().len(),
            1,
            "libraries are keyed by server and must survive an account switch"
        );
        assert!(
            progress::get(&pool, &added.account_id, &added.server_id, "item-1").await.unwrap().is_none(),
            "the old account's local progress cascades away with its row"
        );
        assert_eq!(servers::list(&pool).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn relogin_to_a_different_server_removes_the_old_server_rows_and_files() {
        // Two distinct mock servers: the seed session lives on A, the re-login points at B.
        let server_a = MockServer::start().await;
        let server_b = MockServer::start().await;
        Mock::given(method("POST")).and(path("/login")).respond_with(mock_login_success()).mount(&server_a).await;
        Mock::given(method("POST")).and(path("/login")).respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({
                "user": { "id": "user-2", "username": "bob", "accessToken": "bob-token", "refreshToken": None::<String> },
            }),
        )).mount(&server_b).await;

        let pool = pool().await;
        let added = add_server_and_login(&pool, &server_a.uri(), "jane", "hunter2", &NoPasswords, false).await.unwrap();
        seed_cache(&pool, &added.server_id, &added.account_id).await;
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path().join("data"), tmp.path().join("cache"), tmp.path().join("state"));
        let cover = paths.cover_cache_path(&added.server_id, "item-1", "jpg");
        tokio::fs::create_dir_all(cover.parent().unwrap()).await.unwrap();
        tokio::fs::write(&cover, b"bytes").await.unwrap();

        // The username is deliberately the same as the seed's — the URL alone must drive the
        // server switch.
        let previous = ReloginSeed {
            server_id: added.server_id.clone(),
            account_id: added.account_id.clone(),
            url: server_a.uri(),
            username: "jane".into(),
        };
        let relogged = relogin(&pool, &paths, &previous, &server_b.uri(), "bob", "hunter2", &NoPasswords, false).await.unwrap();

        assert_ne!(relogged.server_id, added.server_id);
        assert!(servers::get(&pool, &added.server_id).await.is_err(), "the old server row must go");
        assert!(accounts::get(&pool, &added.account_id).await.is_err(), "the old account must go with it");
        assert!(
            libraries::list_for_server(&pool, &added.server_id).await.unwrap().is_empty(),
            "the old server's cached rows cascade away"
        );
        assert_eq!(accounts::get(&pool, &relogged.account_id).await.unwrap().username, "bob");
        assert!(accounts::get(&pool, &relogged.account_id).await.unwrap().is_active);
        assert!(!cover.exists(), "the old server's on-disk covers must be purged, not orphaned");
    }

    #[tokio::test]
    async fn relogin_failure_leaves_the_old_session_intact() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/login")).respond_with(mock_login_success()).up_to_n_times(1).mount(&server).await;

        let pool = pool().await;
        let added = add_server_and_login(&pool, &server.uri(), "jane", "hunter2", &NoPasswords, false).await.unwrap();
        let previous = ReloginSeed {
            server_id: added.server_id.clone(),
            account_id: added.account_id.clone(),
            url: server.uri(),
            username: "jane".into(),
        };
        Mock::given(method("POST")).and(path("/login")).respond_with(ResponseTemplate::new(401)).mount(&server).await;

        let result = relogin(&pool, &AppPaths::rooted_at("/tmp/unused", "/tmp/unused", "/tmp/unused"), &previous, &server.uri(), "jane", "wrong", &NoPasswords, false).await;

        assert!(matches!(result, Err(CoreError::Login(_))));
        assert!(accounts::get(&pool, &added.account_id).await.is_ok(), "the old account must survive a failed re-login");
        assert!(accounts::get(&pool, &added.account_id).await.unwrap().is_active, "and must still be the active one");
        assert_eq!(servers::list(&pool).await.unwrap().len(), 1, "no new server row from a failed re-login");
    }

    #[tokio::test]
    async fn a_login_remembers_the_password_only_when_asked_to() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/login")).respond_with(mock_login_success()).mount(&server).await;
        let pool = pool().await;
        let passwords = MemoryPasswords::default();

        let remembered = add_server_and_login(&pool, &server.uri(), "jane", "hunter2", &passwords, true).await.unwrap();
        assert_eq!(passwords.get(&remembered.account_id).as_deref(), Some("hunter2"));

        let previous = ReloginSeed {
            server_id: remembered.server_id.clone(),
            account_id: remembered.account_id.clone(),
            url: server.uri(),
            username: "jane".into(),
        };
        let unpaths = AppPaths::rooted_at("/tmp/unused", "/tmp/unused", "/tmp/unused");
        relogin(&pool, &unpaths, &previous, &server.uri(), "jane", "hunter2", &passwords, false).await.unwrap();
        assert_eq!(passwords.get(&remembered.account_id), None, "signing in with the box unticked forgets it");
    }

    #[tokio::test]
    async fn a_relogin_that_replaces_the_account_forgets_the_old_password() {
        let server_a = MockServer::start().await;
        let server_b = MockServer::start().await;
        Mock::given(method("POST")).and(path("/login")).respond_with(mock_login_success()).mount(&server_a).await;
        Mock::given(method("POST")).and(path("/login")).respond_with(mock_login_success()).mount(&server_b).await;
        let pool = pool().await;
        let passwords = MemoryPasswords::default();
        let added = add_server_and_login(&pool, &server_a.uri(), "jane", "hunter2", &passwords, true).await.unwrap();
        let previous = ReloginSeed {
            server_id: added.server_id.clone(),
            account_id: added.account_id.clone(),
            url: server_a.uri(),
            username: "jane".into(),
        };
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path().join("data"), tmp.path().join("cache"), tmp.path().join("state"));

        let relogged = relogin(&pool, &paths, &previous, &server_b.uri(), "jane", "other-pass", &passwords, true).await.unwrap();

        assert_eq!(passwords.get(&added.account_id), None, "the removed account's password goes with it");
        assert_eq!(passwords.get(&relogged.account_id).as_deref(), Some("other-pass"));
    }

    #[tokio::test]
    async fn signing_out_or_removing_the_server_forgets_the_password() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/login")).respond_with(mock_login_success()).mount(&server).await;
        let pool = pool().await;
        let passwords = MemoryPasswords::default();

        let signed_out = add_server_and_login(&pool, &server.uri(), "jane", "hunter2", &passwords, true).await.unwrap();
        sign_out(&pool, &signed_out.account_id, &passwords).await.unwrap();
        assert_eq!(passwords.get(&signed_out.account_id), None);

        let removed = add_server_and_login(&pool, &server.uri(), "jane", "hunter2", &passwords, true).await.unwrap();
        remove_server(&pool, &removed.server_id, &passwords).await.unwrap();
        assert_eq!(passwords.get(&removed.account_id), None);
    }
}
