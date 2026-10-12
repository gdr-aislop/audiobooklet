//! Keeps a stored account's access token working across launches and across a long session.
//! Audiobookshelf servers from v2.26.0 on issue short-lived JWT access tokens (1–12 hours
//! depending on version/settings) paired with a long-lived refresh token that **rotates on every
//! use** — so a client like this one, which persists tokens across runs and can sit open for
//! hours, must: notice an expiring access token, exchange the refresh token at
//! `POST /auth/refresh`, and persist the rotated pair. Skipping this is exactly the "logged out
//! in ~an hour" behavior this module exists to prevent (observed live: login at ~18:05, every
//! API call 401 by 19:03).
//!
//! Layering: [`Session`] lives in `abs-core` because it is domain logic (DB + HTTP, no widgets);
//! it depends only on `abs-storage` and `abs-api`. The `app` crate constructs one per login and
//! hands it to screens, which ask it for a token at *call* time instead of capturing
//! `account.token` once at build time — the capture-once pattern is what let tokens die
//! mid-session. Everything is deliberately best-effort in the same posture as
//! `covers`/`progress_sync`: if a refresh can't be attempted (legacy server, no refresh token)
//! or fails (server down, session revoked), the current token is returned and the failure is
//! only logged — the next 401 is no worse than what would happen without this module.
//!
//! The one failure that isn't harmless is a refresh the server acted on whose reply never
//! arrived: the server has swapped the pair, and this client still holds the refresh token it
//! replaced. The server keeps accepting that old token for a grace window (10 minutes by
//! default) and answers it with the new pair; after that, the login is gone for good (observed
//! live: a reply cut off at 15:45, the next attempt at 19:32 rejected, signed out). So a refresh
//! that may have been acted on is retried in the background until it's confirmed, inside that
//! window, and the device is kept awake meanwhile so a sleeping phone doesn't let it pass.
//!
//! When the server does reject the refresh token for good (that window missed, the token's
//! 30-day lifetime run out, the server's sessions lost), the session signs in again with the
//! account's remembered password, if there is one (see [`crate::passwords`]), so one sign-in lasts
//! however long the app goes unused. A password the server rejects isn't tried again until the
//! user signs in by hand.

use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;

use abs_storage::models::Account;

/// A JWT is only "fresh enough" if it stays valid through this margin — a token 30 seconds from
/// expiry would otherwise still race the server's clock by the time a request lands.
const FRESH_MARGIN_SECONDS: i64 = 60;

/// After a failed refresh, how long callers get the stored token straight away instead of trying
/// again. Without it every caller queued on the tokens lock retried in turn, each with a full
/// timeout on a bad connection — on the phone four of them chained into ~40 s of waiting before
/// a downloaded book could start.
const REFRESH_RETRY_AFTER: Duration = Duration::from_secs(60);

/// How long after a refresh that may have been acted on (see the module docs) it is still retried
/// in the background: inside the server's grace window for the refresh token it replaced
/// (`REFRESH_TOKEN_GRACE_PERIOD`, 10 minutes by default), with a margin.
const RECOVERY_WINDOW: Duration = Duration::from_secs(9 * 60);

/// The waits between background retries of an unconfirmed refresh; the last one repeats until it
/// is confirmed or [`RECOVERY_WINDOW`] runs out.
const RECOVERY_DELAYS: &[Duration] = &[Duration::from_secs(5), Duration::from_secs(15), Duration::from_secs(30), Duration::from_secs(60)];

/// What the device is told it's being kept awake for, while a refresh is in flight or retried.
const KEEP_AWAKE_REASON: &str = "Refreshing the server login";

/// Keeps the device out of suspend while held. The app supplies it (this crate can't name GTK);
/// see [`Session::set_keep_awake`].
pub trait KeepAwake: Send + Sync {
    /// Starts keeping the device awake; it stops when the returned value is dropped.
    fn hold(&self, reason: &'static str) -> Box<dyn Send>;
}

/// The access token's `exp` claim, in Unix seconds. `None` when this isn't an expiring JWT at all
/// (legacy servers' permanent tokens have no such claim — those need no refreshing), or the
/// payload doesn't parse, which this treats the same way: "can't tell, don't touch it".
pub fn jwt_exp_seconds(token: &str) -> Option<i64> {
    use base64::Engine;

    let payload = token.split('.').nth(1)?;
    let payload = payload.trim_end_matches('=');
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
    let json: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    json.get("exp")?.as_i64()
}

/// Whether an access token stays valid through [`FRESH_MARGIN_SECONDS`]; a token without an `exp`
/// claim never expires.
fn is_fresh(token: &str) -> bool {
    match jwt_exp_seconds(token) {
        None => true,
        Some(exp) => exp > chrono::Utc::now().timestamp() + FRESH_MARGIN_SECONDS,
    }
}

/// How long a local-address probe result stays trusted. Long enough that a sync cycle or a
/// multi-track download doesn't re-probe per call; short enough that switching networks (leaving
/// home Wi-Fi) is noticed on the next probe after the TTL lapses.
const PROBE_TTL: Duration = Duration::from_secs(60);

/// One logged-in session's live token pair: seeded from the persisted [`Account`], refreshed
/// (and persisted back) whenever a caller asks for a token that is expired or about to be.
/// Cheap to clone — every holder sees the same refreshed pair, and a tokio mutex makes
/// concurrent callers share one refresh rather than stampeding the server.
#[derive(Clone)]
pub struct Session {
    inner: Arc<SessionInner>,
}

struct SessionInner {
    pool: SqlitePool,
    server_url: String,
    server_id: String,
    account_id: String,
    username: String,
    tokens: tokio::sync::Mutex<Tokens>,
    /// A copy of the current access token readable without waiting for a refresh in progress —
    /// see [`Session::stored_access_token`]. Updated whenever `tokens` is.
    stored_token: std::sync::Mutex<String>,
    /// Last local-address probe result for this session's server — address, verdict, when.
    /// Scoped to the session (not a process-global) so a probe can never outlive the
    /// connection it was made for, and so tests sharing a process can't leak verdicts to each
    /// other through a global cache.
    probe_cache: tokio::sync::Mutex<Option<CachedProbe>>,
    /// The last `abs_api::Client` minted for this session, plus the exact `(ConnectionTarget,
    /// access token)` it was minted for — see [`Session::api_client`].
    client_cache: tokio::sync::Mutex<Option<CachedClient>>,
    /// The app's offline mode (see [`Session::set_offline`]): while set, nothing minted through
    /// this session reaches the server.
    offline: std::sync::atomic::AtomicBool,
    /// See [`Session::set_keep_awake`].
    keep_awake: std::sync::OnceLock<Arc<dyn KeepAwake>>,
    /// See [`Session::set_password_store`].
    password_store: std::sync::OnceLock<Arc<dyn crate::passwords::PasswordStore>>,
    /// [`RECOVERY_DELAYS`], shortened by tests.
    recovery_delays: &'static [Duration],
}

#[derive(Clone)]
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    /// When the last refresh failed, cleared by a successful one — see [`REFRESH_RETRY_AFTER`].
    refresh_failed_at: Option<std::time::Instant>,
    /// Whether the cool-down since `refresh_failed_at` has been logged yet: once, not per caller.
    cool_down_logged: bool,
    /// When the earliest refresh the server may have acted on without this client getting the
    /// new pair was sent; cleared once a refresh is confirmed or definitely rejected, or
    /// [`RECOVERY_WINDOW`] after it. While set, refreshes are retried in the background.
    unconfirmed: Option<Unconfirmed>,
    /// Whether the background retry task is running.
    recovering: bool,
    /// Whether the server refused the remembered password; it isn't tried again by this session.
    password_rejected: bool,
    /// The pair the database held when this session last read or wrote it. A different stored
    /// pair was written by another session (see [`Session::adopt_newer_stored_pair`]); this
    /// session's own pair differing from it only means persisting that pair failed.
    stored_pair: (String, Option<String>),
}

/// When an unconfirmed refresh was sent, on both clocks: the wall clock keeps counting while the
/// device sleeps, as the server's grace window does, and the monotonic one can't be set back.
#[derive(Clone, Copy)]
struct Unconfirmed {
    at: std::time::SystemTime,
    at_instant: std::time::Instant,
}

impl Unconfirmed {
    fn now() -> Self {
        Self { at: std::time::SystemTime::now(), at_instant: std::time::Instant::now() }
    }

    fn window_over(&self) -> bool {
        let wall = std::time::SystemTime::now().duration_since(self.at).unwrap_or_default();
        wall.max(self.at_instant.elapsed()) >= RECOVERY_WINDOW
    }
}

struct CachedProbe {
    address: String,
    reachable: bool,
    at: std::time::Instant,
}

/// A minted `abs_api::Client`, tagged with exactly what it was minted for — comparing these two
/// fields is how [`Session::api_client`] knows whether the cached client is still good, without
/// having to inspect the client itself (which owns an opaque `reqwest::Client`).
struct CachedClient {
    target: crate::connection::ConnectionTarget,
    access_token: String,
    client: abs_api::Client,
}

impl Session {
    /// Builds a session for an account on its server. `server` is the row the account belongs
    /// to — its URL is the base every call goes through, and its connection settings (custom
    /// headers, TLS, ...) are honored by everything minted through this session. Holding the
    /// row rather than a bare URL keeps one source of truth: there is no second "server URL"
    /// that could drift from what the database says.
    pub fn new(pool: SqlitePool, server: &abs_storage::models::Server, account: &Account) -> Self {
        Self {
            inner: Arc::new(SessionInner {
                pool,
                server_url: server.url.clone(),
                server_id: server.id.clone(),
                account_id: account.id.clone(),
                username: account.username.clone(),
                tokens: tokio::sync::Mutex::new(Tokens {
                    access_token: account.token.clone(),
                    refresh_token: account.refresh_token.clone(),
                    refresh_failed_at: None,
                    cool_down_logged: false,
                    unconfirmed: None,
                    recovering: false,
                    password_rejected: false,
                    stored_pair: (account.token.clone(), account.refresh_token.clone()),
                }),
                stored_token: std::sync::Mutex::new(account.token.clone()),
                probe_cache: tokio::sync::Mutex::new(None),
                client_cache: tokio::sync::Mutex::new(None),
                offline: std::sync::atomic::AtomicBool::new(false),
                keep_awake: std::sync::OnceLock::new(),
                password_store: std::sync::OnceLock::new(),
                recovery_delays: RECOVERY_DELAYS,
            }),
        }
    }

    /// Hands the session the app's way of keeping the device awake, held for every token refresh
    /// and for the background retries of an unconfirmed one. A phone that suspends with a
    /// refresh in flight can lose the reply, and with it the only copy of the new token pair.
    /// Only the first call counts; without one, refreshes run unguarded.
    pub fn set_keep_awake(&self, keep_awake: Arc<dyn KeepAwake>) {
        let _ = self.inner.keep_awake.set(keep_awake);
    }

    /// Hands the session the app's remembered passwords, used to sign in again when the server
    /// rejects the refresh token for good (see the module docs). Only the first call counts;
    /// without one, a rejected refresh token means signing in by hand.
    pub fn set_password_store(&self, store: Arc<dyn crate::passwords::PasswordStore>) {
        let _ = self.inner.password_store.set(store);
    }

    fn keep_awake(&self) -> Option<Box<dyn Send>> {
        self.inner.keep_awake.get().map(|keep_awake| keep_awake.hold(KEEP_AWAKE_REASON))
    }

    pub fn account_id(&self) -> &str {
        &self.inner.account_id
    }

    pub fn server_id(&self) -> &str {
        &self.inner.server_id
    }

    /// Turns the app's offline mode on or off for everything using this session (all clones share
    /// it). While on, [`Session::connection_target`] refuses with [`CoreError::Offline`] — every
    /// server call in the app resolves its connection there, so this is the one gate — and
    /// [`Session::access_token`] doesn't refresh.
    pub fn set_offline(&self, offline: bool) {
        self.inner.offline.store(offline, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_offline(&self) -> bool {
        self.inner.offline.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The connection settings as stored, resolved without any network (no local-address probe):
    /// for building stream URLs and transport properties that won't be used to reach the server
    /// while offline. Never refused by offline mode.
    pub async fn local_connection_target(&self) -> crate::error::Result<crate::connection::ConnectionTarget> {
        let server = abs_storage::repo::servers::get(&self.inner.pool, &self.inner.server_id).await?;
        Ok(crate::connection::ConnectionTarget::resolve(&server, None))
    }

    /// The connection every server-facing call for this session's server should go through.
    /// Fetched fresh from the database, so a settings change made on the Connection page is
    /// picked up by the next sync, download or playback without any rebuild — the same
    /// ask-at-call-time posture this module applies to tokens. The local-address reachability
    /// probe (when a local address is configured) runs through a short-lived per-session cache:
    /// concurrent callers share one probe, the way they share one token refresh.
    pub async fn connection_target(&self) -> crate::error::Result<crate::connection::ConnectionTarget> {
        if self.is_offline() {
            return Err(crate::error::CoreError::Offline);
        }
        let server = abs_storage::repo::servers::get(&self.inner.pool, &self.inner.server_id).await?;

        let Some(local_address) = server
            .local_network_address
            .as_deref()
            .map(str::trim)
            .filter(|address| !address.is_empty())
            .map(str::to_string)
        else {
            return Ok(crate::connection::ConnectionTarget::resolve(&server, None));
        };

        let mut cache = self.inner.probe_cache.lock().await;
        let cached_reachable = cache
            .as_ref()
            .filter(|probe| probe.address == local_address && probe.at.elapsed() < PROBE_TTL)
            .map(|probe| probe.reachable);
        let reachable = match cached_reachable {
            Some(reachable) => reachable,
            None => {
                // Holding the lock across the probe (a tokio mutex, so this is legal) makes
                // concurrent callers wait for — and then reuse — one shared probe result.
                let reachable = crate::connection::probe_reachable(&local_address).await;
                *cache = Some(CachedProbe { address: local_address.clone(), reachable, at: std::time::Instant::now() });
                reachable
            }
        };
        Ok(crate::connection::ConnectionTarget::resolve(&server, Some(reachable)))
    }

    /// The access token as it stands, never refreshed and never waiting — not even for a refresh
    /// in progress. For work that can go ahead without the server (starting a downloaded book)
    /// and only needs a token to build URLs it may never use.
    pub fn stored_access_token(&self) -> String {
        self.inner.stored_token.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// A current access token, refreshing first when the stored one is expired or within
    /// [`FRESH_MARGIN_SECONDS`] of it. Infallible by design: on a failed refresh the existing
    /// token comes back (the caller's request will surface the 401, if any, as its own error)
    /// and the failure is logged. Holding the tokens lock across the refresh makes concurrent
    /// callers wait for — and then reuse — one shared refresh instead of racing several, and a
    /// failed refresh isn't tried again for [`REFRESH_RETRY_AFTER`].
    pub async fn access_token(&self) -> String {
        let mut tokens = self.inner.tokens.lock().await;
        let fresh = is_fresh(&tokens.access_token);
        // Offline: the stored token is all there is; a refresh would be a server call.
        if fresh || self.is_offline() {
            return tokens.access_token.clone();
        }

        // Another session on this account (a second window, a second copy of the app) may have
        // refreshed already, rotating the refresh token this one holds; its pair is the one the
        // server accepts now.
        if self.adopt_newer_stored_pair(&mut tokens).await && is_fresh(&tokens.access_token) {
            return tokens.access_token.clone();
        }

        let refresh_token = match &tokens.refresh_token {
            Some(refresh_token) => refresh_token.clone(),
            None => {
                // Legacy server (permanent tokens don't carry `exp`) or an account that never
                // had one: nothing to exchange.
                return tokens.access_token.clone();
            }
        };

        // A refresh that just failed (often the server not answering at all) won't do better
        // straight away; the callers that queued behind it shouldn't each wait for it again.
        if let Some(failed_at) = tokens.refresh_failed_at {
            let ago = failed_at.elapsed();
            if ago < REFRESH_RETRY_AFTER {
                if !tokens.cool_down_logged {
                    tokens.cool_down_logged = true;
                    tracing::info!(
                        account_id = %self.inner.account_id,
                        "skipping the token refresh: the last attempt failed {}s ago",
                        ago.as_secs()
                    );
                }
                return tokens.access_token.clone();
            }
        }

        self.refresh_locked(&mut tokens, &refresh_token).await;
        tokens.access_token.clone()
    }

    /// One refresh attempt, with the tokens lock held by the caller. Updates `tokens` with the
    /// outcome; when the server may have acted on a request whose reply didn't arrive, marks the
    /// refresh unconfirmed and starts the background retries (see the module docs).
    async fn refresh_locked(&self, tokens: &mut Tokens, refresh_token: &str) {
        let _awake = self.keep_awake();
        let sent = Unconfirmed::now();

        // The refresh call must reach the server the same way everything else does — a
        // self-signed server's refresh can't be verified against the system CA store any more
        // than its sync calls can. Both resolve steps are best-effort here (this function is
        // infallible by design): if the connection can't be resolved or minted, the refresh
        // falls back to a default client, the same posture as every other failed refresh —
        // the failure is logged and the next 401 is no worse than before this module existed.
        let client = match self.connection_target().await {
            Ok(target) => target.plain_client().unwrap_or_else(|err| {
                tracing::warn!(%err, "couldn't mint a client honoring the connection settings; token refresh will use the defaults");
                abs_api::Client::with_default_options(&self.inner.server_url)
            }),
            Err(err) => {
                tracing::warn!(%err, "couldn't load the server's connection settings; token refresh will use the defaults");
                abs_api::Client::with_default_options(&self.inner.server_url)
            }
        };
        match client.refresh(refresh_token).await {
            Ok(result) => {
                // Rotation: the response's refresh token replaces the old one. Servers that
                // don't rotate (or pre-date rotation semantics) may omit it — keeping the
                // existing token is the safe fallback there, and is also what the server's own
                // grace window (v2.35.0+) tolerates.
                tracing::info!(account_id = %self.inner.account_id, recovered = tokens.unconfirmed.is_some(), "refreshed the account's access token");
                self.adopt_pair(tokens, result.access_token, result.refresh_token).await;
            }
            Err(err) => {
                tokens.refresh_failed_at = Some(std::time::Instant::now());
                tokens.cool_down_logged = false;
                let unconfirmed = err.server_may_have_acted();
                tracing::warn!(
                    %err,
                    account_id = %self.inner.account_id,
                    unconfirmed,
                    "couldn't refresh the access token; the stored token will be used and may be rejected"
                );
                if unconfirmed {
                    tokens.unconfirmed.get_or_insert(sent);
                    if !tokens.recovering {
                        tokens.recovering = self.start_recovery();
                    }
                } else if matches!(err, abs_api::LoginError::SessionExpired) {
                    // The server has answered for this token for good; nothing left to recover.
                    tokens.unconfirmed = None;
                    // Unless another session rotated it in the meantime: then its pair is the
                    // live one, and signing in again would only knock that session out.
                    if !self.adopt_newer_stored_pair(tokens).await {
                        self.sign_in_again(tokens, &client).await;
                    }
                }
            }
        }
    }

    /// Takes the pair stored in the database when it differs from this session's: another session
    /// on the same account (a second window, a second copy of the app on the same data) refreshed
    /// or signed in since this one last did. Refresh tokens rotate on every use, so the stored
    /// pair is the one the server accepts, and refreshing with this session's older token would
    /// soon be rejected for good. Returns whether a newer pair was taken; a failed read takes
    /// nothing.
    async fn adopt_newer_stored_pair(&self, tokens: &mut Tokens) -> bool {
        let stored = match abs_storage::repo::accounts::get(&self.inner.pool, &self.inner.account_id).await {
            Ok(stored) => stored,
            Err(err) => {
                tracing::warn!(%err, account_id = %self.inner.account_id, "couldn't check the stored token pair");
                return false;
            }
        };
        let stored_pair = (stored.token, stored.refresh_token);
        if stored_pair == tokens.stored_pair || (stored_pair.0 == tokens.access_token && stored_pair.1 == tokens.refresh_token) {
            return false;
        }
        tracing::info!(account_id = %self.inner.account_id, "took the token pair another session of this account stored");
        tokens.access_token = stored_pair.0.clone();
        tokens.refresh_token = stored_pair.1.clone();
        tokens.stored_pair = stored_pair;
        tokens.refresh_failed_at = None;
        tokens.unconfirmed = None;
        *self.inner.stored_token.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = tokens.access_token.clone();
        true
    }

    /// Takes a new token pair from the server as the session's own, persisted. A missing refresh
    /// token keeps the current one: servers that don't rotate (or pre-date rotation semantics)
    /// may omit it, which is also what the server's own grace window (v2.35.0+) tolerates.
    async fn adopt_pair(&self, tokens: &mut Tokens, access_token: String, refresh_token: Option<String>) {
        let refresh_token = refresh_token.or_else(|| tokens.refresh_token.clone());
        match abs_storage::repo::accounts::set_tokens(&self.inner.pool, &self.inner.account_id, &access_token, refresh_token.as_deref()).await {
            Ok(()) => tokens.stored_pair = (access_token.clone(), refresh_token.clone()),
            Err(err) => tracing::warn!(%err, "got a new access token but couldn't persist it; it will be renewed again next launch"),
        }
        tokens.access_token = access_token;
        tokens.refresh_token = refresh_token;
        tokens.refresh_failed_at = None;
        tokens.unconfirmed = None;
        *self.inner.stored_token.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = tokens.access_token.clone();
    }

    /// After the server rejected the refresh token for good: signs in with the remembered
    /// password, if there is one, through the same `client` the refresh used. A failed attempt
    /// leaves the cool-down set by the refresh in place, so it's tried again no sooner than a
    /// refresh would be; a rejected password isn't tried again at all.
    async fn sign_in_again(&self, tokens: &mut Tokens, client: &abs_api::Client) {
        if tokens.password_rejected {
            return;
        }
        let Some(store) = self.inner.password_store.get() else { return };
        let password = match store.load(&self.inner.account_id).await {
            Ok(Some(password)) => password,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!(%err, account_id = %self.inner.account_id, "couldn't read the remembered password to sign in again");
                return;
            }
        };
        match client.login(&self.inner.username, &password).await {
            Ok(result) => {
                tracing::info!(account_id = %self.inner.account_id, "signed in again with the remembered password");
                self.adopt_pair(tokens, result.access_token, result.refresh_token).await;
            }
            Err(abs_api::LoginError::InvalidCredentials) => {
                tokens.password_rejected = true;
                tracing::warn!(account_id = %self.inner.account_id, "the server refused the remembered password; signing in by hand is needed");
            }
            Err(err) => {
                tracing::warn!(%err, account_id = %self.inner.account_id, "couldn't sign in again with the remembered password; will try again");
            }
        }
    }

    /// Spawns the background retries of an unconfirmed refresh, keeping the device awake until
    /// they end. Holds the session weakly, so signing out ends them too. Returns whether they
    /// started (they can't without an async runtime to run on).
    fn start_recovery(&self) -> bool {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(account_id = %self.inner.account_id, "no async runtime to retry the unconfirmed token refresh on");
            return false;
        };
        tracing::info!(
            account_id = %self.inner.account_id,
            "the server may have replaced the refresh token without this app getting the new one; retrying while it still accepts the old one"
        );
        let session = Arc::downgrade(&self.inner);
        let delays = self.inner.recovery_delays;
        let awake = self.keep_awake();
        runtime.spawn(async move {
            let _awake = awake;
            for attempt in 0.. {
                tokio::time::sleep(delays[attempt.min(delays.len() - 1)]).await;
                let Some(inner) = session.upgrade() else { return };
                if !(Session { inner }).recovery_attempt().await {
                    return;
                }
            }
        });
        true
    }

    /// One background retry of an unconfirmed refresh. Returns whether to keep retrying.
    async fn recovery_attempt(&self) -> bool {
        let mut tokens = self.inner.tokens.lock().await;
        let Some(unconfirmed) = tokens.unconfirmed else {
            // Settled by another caller's refresh in the meantime.
            tokens.recovering = false;
            return false;
        };
        if unconfirmed.window_over() {
            tracing::warn!(
                account_id = %self.inner.account_id,
                "couldn't confirm the token refresh in time; if the server replaced the refresh token, signing in again will be needed"
            );
            tokens.unconfirmed = None;
            tokens.recovering = false;
            return false;
        }
        // Offline mode allows no server calls; wait for it to end while the window lasts.
        if self.is_offline() {
            return true;
        }
        // Another session of this account got the new pair (its refresh with the same old token
        // was answered within the grace window): nothing left to recover.
        if self.adopt_newer_stored_pair(&mut tokens).await {
            tokens.recovering = false;
            return false;
        }
        let Some(refresh_token) = tokens.refresh_token.clone() else {
            tokens.unconfirmed = None;
            tokens.recovering = false;
            return false;
        };
        self.refresh_locked(&mut tokens, &refresh_token).await;
        if tokens.unconfirmed.is_none() {
            tokens.recovering = false;
            return false;
        }
        true
    }

    /// A ready-to-use authenticated `abs_api::Client` for this session's server, reused across
    /// calls as long as the resolved connection and the current access token haven't changed
    /// since the last mint. Minting a client from scratch does a full CA-store parse and TLS
    /// config build (and the first request on it a fresh TCP+TLS handshake, since a brand-new
    /// client starts with an empty connection pool) — real, measurable CPU, and on a phone a
    /// radio wakeup too. That cost is fine once per user action, but callers that mint on a
    /// timer (the playback-progress sync, most notably) used to pay it every single tick; this is
    /// the shared cache that stops that. `access_token()` and `connection_target()` are still
    /// asked fresh on every call — this only skips *minting a new client* when both come back
    /// unchanged, so a token refresh or a settings edit is still picked up immediately.
    ///
    /// Callers that need a specific timeout (covers, downloads — deliberately short so a bad
    /// connection isn't felt as a hang) keep minting directly via `ConnectionTarget::api_client`/
    /// `api_client_with_timeout`, uncached; this is for the standard-timeout, called-often case.
    pub async fn api_client(&self) -> crate::error::Result<abs_api::Client> {
        let target = self.connection_target().await?;
        let access_token = self.access_token().await;

        let mut cache = self.inner.client_cache.lock().await;
        if let Some(cached) = cache.as_ref() {
            if cached.target == target && cached.access_token == access_token {
                return Ok(cached.client.clone());
            }
        }

        let client = target
            .api_client(&access_token)
            .map_err(|err| crate::error::CoreError::ConnectionSetup(err.to_string()))?;
        *cache = Some(CachedClient { target, access_token, client: client.clone() });
        Ok(client)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A JWT-shaped token (`header.payload.signature`) whose payload carries the given `exp`.
    /// Signature is fake — this client never verifies signatures (only the server does).
    fn jwt_with_exp(exp: i64) -> String {
        let payload = serde_json::json!({ "sub": "user-1", "exp": exp });
        let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        format!(
            "{}.{}.{}",
            encode(br#"{"alg":"HS256"}"#),
            encode(payload.to_string().as_bytes()),
            encode(b"signature")
        )
    }

    /// A migrated pool plus one server row pointing at `server_url` (the mock the test is
    /// about to talk to) and one account on it.
    async fn pool_with_account(server_url: &str, token: &str, refresh_token: Option<&str>) -> (SqlitePool, Account, String) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = abs_storage::connect_and_migrate(&tmp.path().join("db.sqlite3")).await.unwrap();
        std::mem::forget(tmp);
        let server_id = abs_storage::repo::servers::add(&pool, server_url).await.unwrap();
        let account_id = abs_storage::repo::accounts::add(&pool, &server_id, "jane", token, refresh_token).await.unwrap();
        let account = abs_storage::repo::accounts::get(&pool, &account_id).await.unwrap();
        (pool, account, server_id)
    }

    impl Session {
        fn with_recovery_delays(mut self, delays: &'static [Duration]) -> Self {
            Arc::get_mut(&mut self.inner).expect("before the session is shared").recovery_delays = delays;
            self
        }
    }

    const FAST_RETRIES: &[Duration] = &[Duration::from_millis(20)];

    /// How one connection to [`scripted_refresh_server`] is answered.
    enum Reply {
        /// Reads the request, then hangs up without answering: the server may have acted on it.
        HangUp,
        Ok(serde_json::Value),
        Rejected,
    }

    fn new_pair() -> serde_json::Value {
        serde_json::json!({ "user": { "id": "user-1", "username": "jane", "accessToken": "new-access", "refreshToken": "new-refresh" } })
    }

    /// An HTTP server answering one connection per entry of `replies`, in order. Returns its URL
    /// and the `x-refresh-token` each request carried.
    fn scripted_refresh_server(replies: Vec<Reply>) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_by_server = seen.clone();
        std::thread::spawn(move || {
            for reply in replies {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).is_ok_and(|n| n == 1) {
                    request.push(byte[0]);
                }
                let request = String::from_utf8_lossy(&request).to_lowercase();
                let token = request.lines().find_map(|line| line.strip_prefix("x-refresh-token: ")).unwrap_or_default();
                seen_by_server.lock().unwrap().push(token.to_string());
                let (status, body) = match reply {
                    Reply::HangUp => continue,
                    Reply::Ok(body) => ("200 OK", body.to_string()),
                    Reply::Rejected => ("401 Unauthorized", r#"{"error":"Invalid refresh token"}"#.to_string()),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (url, seen)
    }

    async fn wait_until(what: &str, mut condition: impl AsyncFnMut() -> bool) {
        let started = std::time::Instant::now();
        while !condition().await {
            assert!(started.elapsed() < Duration::from_secs(5), "timed out waiting until {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Counts the holds a session takes on the device's wakefulness.
    #[derive(Default)]
    struct CountingKeepAwake {
        active: Arc<std::sync::atomic::AtomicUsize>,
        taken: std::sync::atomic::AtomicUsize,
    }

    impl KeepAwake for CountingKeepAwake {
        fn hold(&self, _reason: &'static str) -> Box<dyn Send> {
            struct Release(Arc<std::sync::atomic::AtomicUsize>);
            impl Drop for Release {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
            self.active.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.taken.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::new(Release(self.active.clone()))
        }
    }

    impl CountingKeepAwake {
        fn active(&self) -> usize {
            self.active.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn taken(&self) -> usize {
            self.taken.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Builds the session from the server row the pool already holds — Session reads its
    /// server data from the row (fresh, at call time), so the row is what must point at the
    /// mock, and this helper asserts that invariant.
    async fn session_for(pool: &SqlitePool, account: &Account) -> Session {
        let server = abs_storage::repo::servers::get(pool, &account.server_id).await.unwrap();
        Session::new(pool.clone(), &server, account)
    }

    #[test]
    fn jwt_exp_seconds_decodes_the_exp_claim() {
        let exp = chrono::Utc::now().timestamp() + 1234;
        assert_eq!(jwt_exp_seconds(&jwt_with_exp(exp)), Some(exp));
    }

    #[test]
    fn jwt_exp_seconds_is_none_for_non_jwt_tokens() {
        assert_eq!(jwt_exp_seconds("plain-legacy-token"), None);
        assert_eq!(jwt_exp_seconds("a.b"), None, "payload that isn't JSON");
        assert_eq!(jwt_exp_seconds(""), None);
    }

    #[tokio::test]
    async fn offline_mode_refuses_the_connection_and_never_refreshes() {
        let mock_server = MockServer::start().await;
        let expired = chrono::Utc::now().timestamp() - 10;
        let (pool, account, _) = pool_with_account(&mock_server.uri(), &jwt_with_exp(expired), Some("refresh")).await;
        let session = session_for(&pool, &account).await;

        session.set_offline(true);
        assert!(matches!(session.connection_target().await, Err(crate::error::CoreError::Offline)));
        assert!(matches!(session.api_client().await, Err(crate::error::CoreError::Offline)));
        assert_eq!(session.access_token().await, account.token, "the stored token, unrefreshed");
        assert!(session.local_connection_target().await.is_ok(), "the stored settings are still readable");
        assert!(session.clone().is_offline(), "clones share the switch");
        assert!(mock_server.received_requests().await.unwrap().is_empty(), "nothing reached the server");

        session.set_offline(false);
        assert!(session.connection_target().await.is_ok());
    }

    #[tokio::test]
    async fn a_fresh_token_is_returned_unchanged_without_any_network_call() {
        let mock_server = MockServer::start().await;
        let exp = chrono::Utc::now().timestamp() + 3600;
        let (pool, account, _) = pool_with_account(&mock_server.uri(), &jwt_with_exp(exp), Some("refresh")).await;
        let session = session_for(&pool, &account).await;

        let token = session.access_token().await;

        assert_eq!(token, account.token);
        assert!(mock_server.received_requests().await.unwrap().is_empty(), "no refresh request should be sent");
    }

    #[tokio::test]
    async fn an_expiring_token_is_refreshed_and_both_rotated_tokens_persisted() {
        let mock_server = MockServer::start().await;
        let expired = chrono::Utc::now().timestamp() - 10;
        let (pool, account, _) = pool_with_account(&mock_server.uri(), &jwt_with_exp(expired), Some("old-refresh")).await;
        Mock::given(method("POST"))
            .and(path("/auth/refresh"))
            .and(header("x-refresh-token", "old-refresh"))
            .and(header("x-return-tokens", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user": {
                    "id": "user-1",
                    "username": "jane",
                    "accessToken": "new-access",
                    "refreshToken": "new-refresh",
                }
            })))
            .mount(&mock_server)
            .await;
        let session = session_for(&pool, &account).await;

        let token = session.access_token().await;

        assert_eq!(token, "new-access");
        let stored = abs_storage::repo::accounts::get(&pool, &account.id).await.unwrap();
        assert_eq!(stored.token, "new-access", "the fresh pair must be persisted, not just returned");
        assert_eq!(stored.refresh_token.as_deref(), Some("new-refresh"), "rotation must be picked up");
    }

    #[tokio::test]
    async fn concurrent_callers_share_one_refresh() {
        let mock_server = MockServer::start().await;
        let expired = chrono::Utc::now().timestamp() - 10;
        let (pool, account, _) = pool_with_account(&mock_server.uri(), &jwt_with_exp(expired), Some("old-refresh")).await;
        Mock::given(method("POST"))
            .and(path("/auth/refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user": { "id": "user-1", "username": "jane", "accessToken": "new-access", "refreshToken": "new-refresh" }
            })))
            .mount(&mock_server)
            .await;
        let session = session_for(&pool, &account).await;

        let fetched = {
            let (a, b, c, d, e) = tokio::join!(session.access_token(), session.access_token(), session.access_token(), session.access_token(), session.access_token());
            vec![a, b, c, d, e]
        };

        assert!(fetched.iter().all(|t| t == "new-access"), "all callers get the refreshed token: {fetched:?}");
        assert_eq!(
            mock_server.received_requests().await.unwrap().len(),
            1,
            "one shared refresh, not one per caller (a refresh token rotates on use — racing refreshes would strand all but one caller)"
        );
    }

    #[tokio::test]
    async fn a_second_session_takes_the_pair_the_first_one_refreshed_instead_of_refreshing_its_stale_token() {
        // Two windows (or two copies of the app) on one account: each has its own Session, and
        // the refresh token rotates on use — the second refreshing with the token the first one
        // already used would be rejected once the server's grace window passes.
        let mock_server = MockServer::start().await;
        let expired = chrono::Utc::now().timestamp() - 10;
        let (pool, account, _) = pool_with_account(&mock_server.uri(), &jwt_with_exp(expired), Some("old-refresh")).await;
        Mock::given(method("POST"))
            .and(path("/auth/refresh"))
            .and(header("x-refresh-token", "old-refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(new_pair()))
            .expect(1)
            .mount(&mock_server)
            .await;
        let first = session_for(&pool, &account).await;
        let second = session_for(&pool, &account).await;

        assert_eq!(first.access_token().await, "new-access");
        assert_eq!(second.access_token().await, "new-access", "the second session takes the stored pair");
        assert_eq!(second.stored_access_token(), "new-access");
        assert_eq!(second.inner.tokens.lock().await.refresh_token.as_deref(), Some("new-refresh"));
    }

    #[tokio::test]
    async fn a_second_session_refreshes_with_the_rotated_refresh_token_when_the_stored_access_token_is_stale_too() {
        let mock_server = MockServer::start().await;
        let expired = chrono::Utc::now().timestamp() - 10;
        let (pool, account, _) = pool_with_account(&mock_server.uri(), &jwt_with_exp(expired), Some("old-refresh")).await;
        let second = session_for(&pool, &account).await;
        // Another session refreshed long enough ago that the access token it stored has expired.
        abs_storage::repo::accounts::set_tokens(&pool, &account.id, &jwt_with_exp(expired + 1), Some("rotated-refresh")).await.unwrap();
        Mock::given(method("POST"))
            .and(path("/auth/refresh"))
            .and(header("x-refresh-token", "rotated-refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(new_pair()))
            .expect(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auth/refresh"))
            .and(header("x-refresh-token", "old-refresh"))
            .respond_with(ResponseTemplate::new(401))
            .expect(0)
            .mount(&mock_server)
            .await;

        assert_eq!(second.access_token().await, "new-access");
        let stored = abs_storage::repo::accounts::get(&pool, &account.id).await.unwrap();
        assert_eq!(stored.refresh_token.as_deref(), Some("new-refresh"));
    }

    #[tokio::test]
    async fn a_pair_that_failed_to_persist_is_not_replaced_by_the_older_stored_one() {
        // The database holding a different pair than the session only means "another session
        // wrote it" when the database changed since this session last saw it.
        let mock_server = MockServer::start().await;
        let expired = chrono::Utc::now().timestamp() - 10;
        let (pool, account, _) = pool_with_account(&mock_server.uri(), &jwt_with_exp(expired), Some("old-refresh")).await;
        let session = session_for(&pool, &account).await;
        {
            let mut tokens = session.inner.tokens.lock().await;
            tokens.access_token = jwt_with_exp(expired + 1);
            tokens.refresh_token = Some("unpersisted-refresh".into());
            assert!(!session.adopt_newer_stored_pair(&mut tokens).await);
            assert_eq!(tokens.refresh_token.as_deref(), Some("unpersisted-refresh"));
        }
    }

    #[tokio::test]
    async fn a_rejected_refresh_takes_a_pair_another_session_stored_instead_of_signing_in_again() {
        let mock_server = MockServer::start().await;
        let expired = chrono::Utc::now().timestamp() - 10;
        let (pool, account, _) = pool_with_account(&mock_server.uri(), &jwt_with_exp(expired), Some("old-refresh")).await;
        let session = session_for(&pool, &account).await;
        session.set_password_store(Arc::new(crate::passwords::MemoryPasswords::with(&account.id, "hunter2")));
        // The other session rotates the pair while this one's refresh is on its way.
        let rotated_pool = pool.clone();
        let account_id = account.id.clone();
        Mock::given(method("POST"))
            .and(path("/auth/refresh"))
            .respond_with(move |_: &wiremock::Request| {
                let pool = rotated_pool.clone();
                let account_id = account_id.clone();
                std::thread::spawn(move || {
                    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(
                        abs_storage::repo::accounts::set_tokens(&pool, &account_id, "other-access", Some("other-refresh")),
                    )
                })
                .join()
                .unwrap()
                .unwrap();
                ResponseTemplate::new(401).set_body_json(serde_json::json!({ "error": "Invalid refresh token" }))
            })
            .mount(&mock_server)
            .await;
        Mock::given(method("POST")).and(path("/login")).respond_with(ResponseTemplate::new(500)).expect(0).mount(&mock_server).await;

        assert_eq!(session.access_token().await, "other-access");
        assert_eq!(session.inner.tokens.lock().await.refresh_token.as_deref(), Some("other-refresh"));
    }

    #[tokio::test]
    async fn a_refresh_response_without_a_new_refresh_token_keeps_the_old_one() {
        let mock_server = MockServer::start().await;
        let expired = chrono::Utc::now().timestamp() - 10;
        let (pool, account, _) = pool_with_account(&mock_server.uri(), &jwt_with_exp(expired), Some("old-refresh")).await;
        Mock::given(method("POST"))
            .and(path("/auth/refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user": { "id": "user-1", "username": "jane", "accessToken": "new-access" }
            })))
            .mount(&mock_server)
            .await;
        let session = session_for(&pool, &account).await;

        let token = session.access_token().await;

        assert_eq!(token, "new-access");
        let stored = abs_storage::repo::accounts::get(&pool, &account.id).await.unwrap();
        assert_eq!(stored.refresh_token.as_deref(), Some("old-refresh"));
    }

    #[tokio::test]
    async fn a_failed_refresh_returns_the_current_token_gracefully() {
        let mock_server = MockServer::start().await;
        let expired = chrono::Utc::now().timestamp() - 10;
        let (pool, account, _) = pool_with_account(&mock_server.uri(), &jwt_with_exp(expired), Some("revoked-refresh")).await;
        Mock::given(method("POST")).and(path("/auth/refresh")).respond_with(ResponseTemplate::new(401)).mount(&mock_server).await;
        let session = session_for(&pool, &account).await;

        let token = session.access_token().await;

        assert_eq!(token, account.token, "the stored token is still what the caller uses");
        let stored = abs_storage::repo::accounts::get(&pool, &account.id).await.unwrap();
        assert_eq!(stored.token, account.token, "nothing is overwritten on a failed refresh");
    }

    /// On the phone a refresh that couldn't reach the server was retried by every caller queued
    /// behind it, each waiting out its own timeout; the player, last in line, started ~40 s late.
    #[tokio::test]
    async fn a_failed_refresh_is_not_retried_by_the_next_caller() {
        let mock_server = MockServer::start().await;
        let expired = chrono::Utc::now().timestamp() - 10;
        let (pool, account, _) = pool_with_account(&mock_server.uri(), &jwt_with_exp(expired), Some("refresh")).await;
        Mock::given(method("POST"))
            .and(path("/auth/refresh"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&mock_server)
            .await;
        let session = session_for(&pool, &account).await;

        assert_eq!(session.access_token().await, account.token);
        let started = std::time::Instant::now();
        let again: Vec<String> = futures::future::join_all((0..3).map(|_| session.access_token())).await;

        assert!(again.iter().all(|t| *t == account.token), "the stored token, unrefreshed: {again:?}");
        assert!(started.elapsed() < Duration::from_millis(500), "no waiting on a second attempt");
        // `expect(1)` is verified when the mock server drops.
    }

    #[tokio::test]
    async fn a_successful_refresh_after_the_cool_down_clears_it() {
        let mock_server = MockServer::start().await;
        let expired = chrono::Utc::now().timestamp() - 10;
        let (pool, account, _) = pool_with_account(&mock_server.uri(), &jwt_with_exp(expired), Some("refresh")).await;
        let session = session_for(&pool, &account).await;
        // As if the last attempt failed longer ago than the cool-down.
        session.inner.tokens.lock().await.refresh_failed_at =
            Some(std::time::Instant::now() - REFRESH_RETRY_AFTER - Duration::from_secs(1));
        Mock::given(method("POST"))
            .and(path("/auth/refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user": { "id": "user-1", "username": "jane", "accessToken": "new-access", "refreshToken": "new-refresh" }
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        assert_eq!(session.access_token().await, "new-access", "tried again once the cool-down was over");
        assert!(session.inner.tokens.lock().await.refresh_failed_at.is_none());
        assert_eq!(session.stored_access_token(), "new-access", "the no-wait copy follows the refresh");
    }

    #[tokio::test]
    async fn the_stored_token_is_returned_without_waiting_for_a_refresh_in_progress() {
        let mock_server = MockServer::start().await;
        let expired = chrono::Utc::now().timestamp() - 10;
        let (pool, account, _) = pool_with_account(&mock_server.uri(), &jwt_with_exp(expired), Some("refresh")).await;
        let session = session_for(&pool, &account).await;
        let _refresh_in_progress = session.inner.tokens.lock().await;

        assert_eq!(session.stored_access_token(), account.token);
    }

    #[tokio::test]
    async fn an_expired_token_with_no_refresh_token_is_left_alone() {
        // Legacy server case: no refresh token to exchange — the call must not hit the network.
        let mock_server = MockServer::start().await;
        let expired = chrono::Utc::now().timestamp() - 10;
        let (pool, account, _) = pool_with_account(&mock_server.uri(), &jwt_with_exp(expired), None).await;
        let session = session_for(&pool, &account).await;

        let token = session.access_token().await;

        assert_eq!(token, account.token);
        assert!(mock_server.received_requests().await.unwrap().is_empty());
    }

    /// The token refresh must reach the server the way *everything* does — through the
    /// server's connection settings. A custom header saved on the Connection page has to ride
    /// along on `/auth/refresh` too, or a reverse-proxy-fronted server would accept syncs but
    /// silently drop every refresh (and with it, the session an hour in).
    #[tokio::test]
    async fn the_refresh_request_honors_the_server_s_connection_settings() {
        let mock_server = MockServer::start().await;
        let expired = chrono::Utc::now().timestamp() - 10;
        let (pool, account, server_id) = pool_with_account(&mock_server.uri(), &jwt_with_exp(expired), Some("old-refresh")).await;
        abs_storage::repo::servers::set_custom_headers_json(&pool, &server_id, r#"{"X-Auth":"secret"}"#).await.unwrap();
        Mock::given(method("POST"))
            .and(path("/auth/refresh"))
            .and(header("x-refresh-token", "old-refresh"))
            .and(header("x-auth", "secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user": { "id": "user-1", "username": "jane", "accessToken": "new-access", "refreshToken": "new-refresh" }
            })))
            .mount(&mock_server)
            .await;
        let session = session_for(&pool, &account).await;

        let token = session.access_token().await;

        assert_eq!(token, "new-access", "the refresh went through only if the custom header was sent");
    }

    /// The failure that signed a real user out: the server swapped the pair, the reply never
    /// arrived, and nothing retried while the server still accepted the old refresh token.
    #[tokio::test]
    async fn a_refresh_whose_reply_was_lost_is_retried_with_the_same_token_until_it_lands() {
        let (url, seen) = scripted_refresh_server(vec![Reply::HangUp, Reply::HangUp, Reply::Ok(new_pair())]);
        let expired = chrono::Utc::now().timestamp() - 10;
        let (pool, account, _) = pool_with_account(&url, &jwt_with_exp(expired), Some("old-refresh")).await;
        let session = session_for(&pool, &account).await.with_recovery_delays(FAST_RETRIES);
        let keep_awake = Arc::new(CountingKeepAwake::default());
        session.set_keep_awake(keep_awake.clone());

        assert_eq!(session.access_token().await, account.token, "the caller isn't held up by the retries");
        wait_until("the retry lands", async || abs_storage::repo::accounts::get(&pool, &account.id).await.unwrap().token == "new-access").await;

        assert_eq!(*seen.lock().unwrap(), ["old-refresh"; 3], "every retry carries the token the server may have replaced");
        let stored = abs_storage::repo::accounts::get(&pool, &account.id).await.unwrap();
        assert_eq!(stored.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(session.stored_access_token(), "new-access");
        wait_until("the retries end", async || !session.inner.tokens.lock().await.recovering).await;
        assert!(session.inner.tokens.lock().await.unconfirmed.is_none());
        wait_until("the device may sleep again", async || keep_awake.active() == 0).await;
        assert!(keep_awake.taken() >= 4, "each attempt and the retries as a whole kept the device awake");
    }

    #[tokio::test]
    async fn a_refresh_that_never_reached_the_server_is_not_retried_in_the_background() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let expired = chrono::Utc::now().timestamp() - 10;
        let (pool, account, _) = pool_with_account(&format!("http://{closed}"), &jwt_with_exp(expired), Some("old-refresh")).await;
        let session = session_for(&pool, &account).await.with_recovery_delays(FAST_RETRIES);

        assert_eq!(session.access_token().await, account.token);

        let tokens = session.inner.tokens.lock().await;
        assert!(tokens.unconfirmed.is_none() && !tokens.recovering, "the old token is still the server's current one");
    }

    #[tokio::test]
    async fn a_rejection_ends_the_retries() {
        let (url, seen) = scripted_refresh_server(vec![Reply::HangUp, Reply::Rejected, Reply::Ok(new_pair())]);
        let expired = chrono::Utc::now().timestamp() - 10;
        let (pool, account, _) = pool_with_account(&url, &jwt_with_exp(expired), Some("old-refresh")).await;
        let session = session_for(&pool, &account).await.with_recovery_delays(FAST_RETRIES);

        session.access_token().await;
        wait_until("the retries end", async || !session.inner.tokens.lock().await.recovering).await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(seen.lock().unwrap().len(), 2, "nothing after the server's definite answer");
        assert!(session.inner.tokens.lock().await.unconfirmed.is_none());
    }

    #[tokio::test]
    async fn the_retries_stop_once_the_server_no_longer_accepts_the_old_token() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/auth/refresh")).respond_with(ResponseTemplate::new(500)).expect(0).mount(&mock_server).await;
        let expired = chrono::Utc::now().timestamp() - 10;
        let (pool, account, _) = pool_with_account(&mock_server.uri(), &jwt_with_exp(expired), Some("old-refresh")).await;
        let session = session_for(&pool, &account).await.with_recovery_delays(FAST_RETRIES);
        {
            let mut tokens = session.inner.tokens.lock().await;
            tokens.unconfirmed = Some(Unconfirmed { at: std::time::SystemTime::now() - RECOVERY_WINDOW, at_instant: std::time::Instant::now() });
            tokens.recovering = session.start_recovery();
        }

        wait_until("the retries end", async || !session.inner.tokens.lock().await.recovering).await;

        assert!(session.inner.tokens.lock().await.unconfirmed.is_none());
        // `expect(0)` is verified when the mock server drops.
    }

    #[tokio::test]
    async fn the_device_is_kept_awake_for_exactly_as_long_as_a_refresh_is_in_flight() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(new_pair()).set_delay(Duration::from_millis(300)))
            .mount(&mock_server)
            .await;
        let expired = chrono::Utc::now().timestamp() - 10;
        let (pool, account, _) = pool_with_account(&mock_server.uri(), &jwt_with_exp(expired), Some("old-refresh")).await;
        let session = session_for(&pool, &account).await;
        let keep_awake = Arc::new(CountingKeepAwake::default());
        session.set_keep_awake(keep_awake.clone());

        let refreshing = tokio::spawn({
            let session = session.clone();
            async move { session.access_token().await }
        });
        wait_until("the refresh is in flight", async || keep_awake.active() == 1).await;

        assert_eq!(refreshing.await.unwrap(), "new-access");
        assert_eq!((keep_awake.active(), keep_awake.taken()), (0, 1), "released once the refresh is done");
    }

    /// A session whose refresh token the server rejects for good (`/auth/refresh` answers 401),
    /// with `jane`'s password remembered as `remembered` (none when `None`).
    async fn rejected_session(mock_server: &MockServer, remembered: Option<&str>) -> (SqlitePool, Account, Session) {
        Mock::given(method("POST")).and(path("/auth/refresh")).respond_with(ResponseTemplate::new(401)).mount(mock_server).await;
        let expired = chrono::Utc::now().timestamp() - 10;
        let (pool, account, _) = pool_with_account(&mock_server.uri(), &jwt_with_exp(expired), Some("dead-refresh")).await;
        let session = session_for(&pool, &account).await;
        let passwords = match remembered {
            Some(password) => crate::passwords::MemoryPasswords::with(&account.id, password),
            None => crate::passwords::MemoryPasswords::default(),
        };
        session.set_password_store(Arc::new(passwords));
        (pool, account, session)
    }

    /// As if the cool-down after the last failure were over.
    async fn skip_the_cool_down(session: &Session) {
        session.inner.tokens.lock().await.refresh_failed_at = Some(std::time::Instant::now() - REFRESH_RETRY_AFTER - Duration::from_secs(1));
    }

    #[tokio::test]
    async fn a_rejected_refresh_token_signs_in_again_with_the_remembered_password() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/login"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({ "username": "jane", "password": "hunter2" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(new_pair()))
            .expect(1)
            .mount(&mock_server)
            .await;
        let (pool, account, session) = rejected_session(&mock_server, Some("hunter2")).await;

        assert_eq!(session.access_token().await, "new-access");

        let stored = abs_storage::repo::accounts::get(&pool, &account.id).await.unwrap();
        assert_eq!(stored.token, "new-access");
        assert_eq!(stored.refresh_token.as_deref(), Some("new-refresh"), "the new session's refresh token takes over");
        assert_eq!(session.stored_access_token(), "new-access");
    }

    #[tokio::test]
    async fn without_a_remembered_password_a_rejected_refresh_token_stays_rejected() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/login")).respond_with(ResponseTemplate::new(200).set_body_json(new_pair())).expect(0).mount(&mock_server).await;
        let (_pool, account, session) = rejected_session(&mock_server, None).await;

        assert_eq!(session.access_token().await, account.token);
    }

    #[tokio::test]
    async fn a_password_the_server_refuses_is_not_tried_again() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/login")).respond_with(ResponseTemplate::new(401)).expect(1).mount(&mock_server).await;
        let (_pool, account, session) = rejected_session(&mock_server, Some("changed-since")).await;

        assert_eq!(session.access_token().await, account.token);
        skip_the_cool_down(&session).await;
        assert_eq!(session.access_token().await, account.token);

        assert!(session.inner.tokens.lock().await.password_rejected);
        // `expect(1)` is verified when the mock server drops.
    }

    #[tokio::test]
    async fn a_sign_in_that_fails_for_another_reason_is_tried_again_after_the_cool_down() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/login")).respond_with(ResponseTemplate::new(502)).up_to_n_times(1).mount(&mock_server).await;
        Mock::given(method("POST")).and(path("/login")).respond_with(ResponseTemplate::new(200).set_body_json(new_pair())).mount(&mock_server).await;
        let (_pool, account, session) = rejected_session(&mock_server, Some("hunter2")).await;

        assert_eq!(session.access_token().await, account.token, "the server was down");
        skip_the_cool_down(&session).await;

        assert_eq!(session.access_token().await, "new-access");
    }
}
