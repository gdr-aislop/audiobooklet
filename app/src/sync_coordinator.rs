//! Prevents Home and Library from both running a full, network-touching startup sync for the
//! same account at the same moment. `screens::main_window::build_main_window` builds both screens
//! eagerly, and each screen's own `build()` kicks off its own automatic sync cycle immediately —
//! without this, every app launch fetched every library's complete item list (plus reconciled
//! progress, plus fetched every cover) twice, concurrently, over two separate connections. Manual
//! triggers (the ⋯ menu's "Sync now", pull-to-refresh) are unaffected: they always run for real,
//! since they're user-initiated and there's no second screen racing them.
//!
//! Deliberately process-global (a `thread_local!`, safe without a `Mutex` since GTK's main loop is
//! single-threaded) rather than an instance threaded through `build()`'s already-long parameter
//! list and every one of its ~150 test call sites — the alternative this module's own review
//! considered and rejected as disproportionate risk for what it would buy. Keyed by
//! `(server_id, account_id)` so it can never affect two different accounts or servers, and each
//! entry is removed the moment its sync completes and every waiter has been notified — so a
//! logout/login (or an account switch) that reuses the same ids later starts a fresh automatic
//! sync rather than being stuck skipping forever.

use std::cell::RefCell;
use std::collections::HashMap;

struct Entry {
    completed: bool,
    waiters: Vec<Box<dyn FnOnce(Option<SyncOutcome>)>>,
}

/// How the claimed sync ended, handed to every screen that waited for it — so the screen that
/// didn't run the sync still says "can't reach your server" (or shows the error page on an empty
/// install) instead of silently redrawing from cache, or spinning forever.
#[derive(Clone)]
pub(crate) enum SyncOutcome {
    Ok,
    Failed(std::rc::Rc<abs_core::CoreError>),
}

impl SyncOutcome {
    pub(crate) fn from_result(result: &Result<(), abs_core::CoreError>) -> Self {
        match result {
            Ok(()) => SyncOutcome::Ok,
            // `CoreError` isn't `Clone`; the waiters only need to read it.
            Err(err) => SyncOutcome::Failed(std::rc::Rc::new(clone_error(err))),
        }
    }

    pub(crate) fn error(&self) -> Option<&abs_core::CoreError> {
        match self {
            SyncOutcome::Ok => None,
            SyncOutcome::Failed(err) => Some(err),
        }
    }
}

/// A copy of `err` good enough for showing it: the variant (which picks the wording) and its text.
fn clone_error(err: &abs_core::CoreError) -> abs_core::CoreError {
    use abs_core::CoreError::*;
    match err {
        Auth => Auth,
        Offline => Offline,
        Unreachable(text) => Unreachable(text.clone()),
        UnexpectedResponse(text) => UnexpectedResponse(text.clone()),
        other => UnexpectedResponse(other.to_string()),
    }
}

thread_local! {
    static CLAIMS: RefCell<HashMap<(String, String), Entry>> = RefCell::new(HashMap::new());
}

/// The first caller for a given `(server_id, account_id)` gets `true` — go ahead and run the
/// real, network-touching sync, then call [`mark_completed`] once it (and everything downstream
/// of it that other screens should wait for, e.g. the cover fetch) has actually landed. Every
/// caller after that, until the claim is cleared by `mark_completed`, gets `false` — skip the
/// network round entirely and register interest via [`on_completed`] instead.
pub(crate) fn claim_startup_sync(server_id: &str, account_id: &str) -> bool {
    CLAIMS.with(|claims| {
        let mut claims = claims.borrow_mut();
        let key = (server_id.to_string(), account_id.to_string());
        match claims.entry(key) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(Entry { completed: false, waiters: Vec::new() });
                true
            }
        }
    })
}

/// Registers a one-shot callback for when the claimed sync for `(server_id, account_id)`
/// finishes; it gets that sync's [`SyncOutcome`]. If it already has finished — or nothing is
/// currently claimed for these ids at all (the defensive case: a caller here without a matching
/// `claim_startup_sync` should not happen, but must never silently drop the follow-up render a
/// caller is relying on) — the callback runs immediately, with `None`: there's no outcome to
/// report any more, only the cache to redraw from.
pub(crate) fn on_completed(server_id: &str, account_id: &str, callback: impl FnOnce(Option<SyncOutcome>) + 'static) {
    let key = (server_id.to_string(), account_id.to_string());
    let mut callback: Option<Box<dyn FnOnce(Option<SyncOutcome>)>> = Some(Box::new(callback));
    CLAIMS.with(|claims| {
        let mut claims = claims.borrow_mut();
        if let Some(entry) = claims.get_mut(&key) {
            if !entry.completed {
                entry.waiters.push(callback.take().expect("callback is always Some here"));
            }
        }
    });
    if let Some(callback) = callback {
        callback(None);
    }
}

/// Called by whichever screen actually claimed the sync for `(server_id, account_id)`, once its
/// full cycle (sync + reconcile + covers, whatever that screen's own `spawn_sync_cycle` considers
/// "done") has landed and rendered, with how it went. Fires every registered [`on_completed`]
/// callback with that outcome and removes the claim, so a later rebuild for the same ids (a
/// relogin, an account switch that comes back around) starts a fresh automatic sync rather than
/// skipping forever.
pub(crate) fn mark_completed(server_id: &str, account_id: &str, outcome: SyncOutcome) {
    let key = (server_id.to_string(), account_id.to_string());
    let waiters = CLAIMS.with(|claims| claims.borrow_mut().remove(&key).map(|entry| entry.waiters).unwrap_or_default());
    for waiter in waiters {
        waiter(Some(outcome.clone()));
    }
}

/// Which screen ran a sync, so a screen isn't told about its own.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Screen {
    Home,
    Library,
}

/// A screen's interest in the other screen's syncs: the callback re-reads local storage and
/// returns whether the screen is still alive (`false` drops the listener, e.g. after the shell
/// was rebuilt for another account).
struct SyncedListener {
    key: (String, String),
    screen: Screen,
    callback: Box<dyn Fn(SyncOutcome) -> bool>,
}

thread_local! {
    static SYNCED_LISTENERS: RefCell<Vec<SyncedListener>> = const { RefCell::new(Vec::new()) };
}

/// Registers `screen`'s interest in syncs the *other* screen runs for `(server_id, account_id)`
/// outside the startup claim — "Sync now", pull-to-refresh, the catch-up after offline mode is
/// turned off. Found in the 2026-10-12 pass: a library added on the server showed up on Home
/// after its "Sync now", but never on the Library tab, which had loaded once and never again.
pub(crate) fn on_synced(server_id: &str, account_id: &str, screen: Screen, callback: impl Fn(SyncOutcome) -> bool + 'static) {
    SYNCED_LISTENERS.with(|listeners| {
        listeners.borrow_mut().push(SyncedListener {
            key: (server_id.to_string(), account_id.to_string()),
            screen,
            callback: Box::new(callback),
        });
    });
}

/// Called by a screen when a sync it ran outside the startup claim has finished: every other
/// screen showing that account re-reads what landed.
pub(crate) fn notify_synced(server_id: &str, account_id: &str, from: Screen, outcome: SyncOutcome) {
    let key = (server_id.to_string(), account_id.to_string());
    // Taken out while the callbacks run, so one can register or notify without a double borrow.
    let listeners = SYNCED_LISTENERS.with(|listeners| std::mem::take(&mut *listeners.borrow_mut()));
    let mut kept = Vec::with_capacity(listeners.len());
    for listener in listeners {
        let alive = if listener.key == key && listener.screen != from { (listener.callback)(outcome.clone()) } else { true };
        if alive {
            kept.push(listener);
        }
    }
    SYNCED_LISTENERS.with(|listeners| {
        let mut listeners = listeners.borrow_mut();
        kept.append(&mut listeners);
        *listeners = kept;
    });
}

/// The item the player has loaded (or is starting) — `(server_id, account_id, item_id)`. Home's
/// and Library's progress reconcile leave that item's row to the player (see
/// `abs_core::progress_sync::reconcile_all_progress`'s `skip_item`). A real `Mutex`, not a
/// `thread_local!`: the reconcile runs on a tokio worker, and asks at the moment it needs the
/// answer rather than when its cycle was started, so a book started mid-sync is still protected.
static LOADED_ITEM: std::sync::Mutex<Option<(String, String, String)>> = std::sync::Mutex::new(None);

/// Called by the player whenever what it holds changes; `None` once nothing is loaded.
pub(crate) fn set_loaded_item(loaded: Option<(&str, &str, &str)>) {
    let loaded = loaded.map(|(server_id, account_id, item_id)| (server_id.to_string(), account_id.to_string(), item_id.to_string()));
    *LOADED_ITEM.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = loaded;
}

/// Clears the loaded item, but only if it is still `(server_id, account_id, item_id)` — a
/// retiring player must not wipe what its successor has since published.
pub(crate) fn clear_loaded_item_if(server_id: &str, account_id: &str, item_id: &str) {
    let mut loaded = LOADED_ITEM.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if loaded.as_ref().is_some_and(|(server, account, item)| server == server_id && account == account_id && item == item_id) {
        *loaded = None;
    }
}

/// The player's loaded item for this server and account, if any.
pub(crate) fn loaded_item(server_id: &str, account_id: &str) -> Option<String> {
    let loaded = LOADED_ITEM.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    loaded.as_ref().filter(|(server, account, _)| server == server_id && account == account_id).map(|(_, _, item)| item.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn first_claim_succeeds_later_ones_do_not() {
        assert!(claim_startup_sync("server-1", "account-1"));
        assert!(!claim_startup_sync("server-1", "account-1"));
        // A different account is a distinct key — never affected by another account's claim.
        assert!(claim_startup_sync("server-1", "account-2"));
        mark_completed("server-1", "account-1", SyncOutcome::Ok);
        mark_completed("server-1", "account-2", SyncOutcome::Ok);
    }

    #[test]
    fn on_completed_runs_once_mark_completed_fires() {
        assert!(claim_startup_sync("server-2", "account-1"));
        let ran = Rc::new(Cell::new(false));
        on_completed("server-2", "account-1", {
            let ran = ran.clone();
            move |_| ran.set(true)
        });
        assert!(!ran.get(), "must not run before the claimed sync actually completes");
        mark_completed("server-2", "account-1", SyncOutcome::Ok);
        assert!(ran.get());
    }

    /// The screen that didn't run the sync must learn that it failed, and how.
    #[test]
    fn a_waiter_receives_the_failed_outcome() {
        assert!(claim_startup_sync("server-6", "account-1"));
        let seen: Rc<std::cell::RefCell<Option<Option<String>>>> = Default::default();
        on_completed("server-6", "account-1", {
            let seen = seen.clone();
            move |outcome| *seen.borrow_mut() = Some(outcome.and_then(|o| o.error().map(|e| e.to_string())))
        });
        let failure: Result<(), abs_core::CoreError> = Err(abs_core::CoreError::Unreachable("connection refused".to_string()));
        mark_completed("server-6", "account-1", SyncOutcome::from_result(&failure));
        assert_eq!(seen.borrow().clone(), Some(Some("couldn't connect to the server: connection refused".to_string())));
    }

    #[test]
    fn on_completed_runs_immediately_if_already_completed() {
        assert!(claim_startup_sync("server-3", "account-1"));
        mark_completed("server-3", "account-1", SyncOutcome::Ok);
        let ran = Rc::new(Cell::new(false));
        on_completed("server-3", "account-1", {
            let ran = ran.clone();
            move |_| ran.set(true)
        });
        assert!(ran.get(), "a caller arriving after completion should render immediately, not wait forever");
    }

    #[test]
    fn on_completed_runs_immediately_if_nothing_was_ever_claimed() {
        let ran = Rc::new(Cell::new(false));
        on_completed("server-4", "account-1", {
            let ran = ran.clone();
            move |_| ran.set(true)
        });
        assert!(ran.get(), "no matching claim must never mean a lost callback");
    }

    #[test]
    fn mark_completed_clears_the_claim_so_a_later_build_syncs_again() {
        assert!(claim_startup_sync("server-5", "account-1"));
        mark_completed("server-5", "account-1", SyncOutcome::Ok);
        assert!(claim_startup_sync("server-5", "account-1"), "a relogin/account-switch reusing the same ids must sync again, not skip forever");
    }

    #[test]
    fn the_loaded_item_is_reported_only_for_its_own_server_and_account() {
        set_loaded_item(Some(("server-9", "account-9", "item-9")));
        assert_eq!(loaded_item("server-9", "account-9").as_deref(), Some("item-9"));
        assert_eq!(loaded_item("server-9", "account-other"), None);
        // A retiring player clears only its own book, never a successor's.
        clear_loaded_item_if("server-9", "account-9", "item-other");
        assert_eq!(loaded_item("server-9", "account-9").as_deref(), Some("item-9"));
        clear_loaded_item_if("server-9", "account-9", "item-9");
        assert_eq!(loaded_item("server-9", "account-9"), None);
        set_loaded_item(None);
        assert_eq!(loaded_item("server-9", "account-9"), None);
    }

    #[test]
    fn a_sync_is_told_to_the_other_screens_of_the_same_account_only() {
        let library_told = Rc::new(Cell::new(0));
        let other_account_told = Rc::new(Cell::new(0));
        on_synced("s-notify", "a-1", Screen::Library, {
            let told = library_told.clone();
            move |_| {
                told.set(told.get() + 1);
                true
            }
        });
        on_synced("s-notify", "a-2", Screen::Library, {
            let told = other_account_told.clone();
            move |_| {
                told.set(told.get() + 1);
                true
            }
        });
        notify_synced("s-notify", "a-1", Screen::Home, SyncOutcome::Ok);
        notify_synced("s-notify", "a-1", Screen::Library, SyncOutcome::Ok);
        assert_eq!(library_told.get(), 1, "told about Home's sync, not its own");
        assert_eq!(other_account_told.get(), 0);
    }

    #[test]
    fn a_listener_whose_screen_is_gone_is_dropped() {
        let calls = Rc::new(Cell::new(0));
        on_synced("s-drop", "a-1", Screen::Home, {
            let calls = calls.clone();
            move |_| {
                calls.set(calls.get() + 1);
                false
            }
        });
        notify_synced("s-drop", "a-1", Screen::Library, SyncOutcome::Ok);
        notify_synced("s-drop", "a-1", Screen::Library, SyncOutcome::Ok);
        assert_eq!(calls.get(), 1);
    }
}
