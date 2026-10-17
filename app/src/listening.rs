//! Listening sessions: recording them as the player plays ([`SessionRecorder`]) and sending them
//! to the server ([`SessionSender`]) — see `abs_core::listening_sessions` for why sessions, and
//! for the batched request itself.
//!
//! When sessions are sent is chosen for a phone's radio, which stays powered for several seconds
//! after every transfer:
//! - every [`SESSION_SYNC_INTERVAL`] while playing (measured from the last send of any kind);
//! - on the moments that matter: a pause (after [`PAUSE_SEND_DELAY`], so a headset blip or a
//!   quick resume costs nothing), the end of the book, a sleep timer, switching books or
//!   accounts, quitting, the app starting and the network coming back;
//! - opportunistically when the radio is awake anyway — a streamed file finished downloading in
//!   burst mode — unless something was sent less than [`OPPORTUNISTIC_GAP`] ago.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use abs_core::listening_sessions::Backoff;
use abs_storage::models::ListeningSession;
use adw::glib;
use sqlx::SqlitePool;

use crate::player::ProgressSyncOutcome;

/// How often sessions are sent while playing, counted from the last send of any kind. Others
/// send every 15–60 s (Audiobookshelf's apps 15 s, Lissen 45 s, Pocket Casts 60 s) or only on
/// pause (AntennaPod); local sessions have no server timeout to keep alive, and the position is
/// saved on the device every few seconds anyway, so a crash only delays the server's copy.
pub const SESSION_SYNC_INTERVAL: Duration = Duration::from_secs(180);

/// A pause is sent this long after it happens, unless playback resumed meanwhile.
pub const PAUSE_SEND_DELAY: Duration = Duration::from_secs(3);

/// An opportunistic send is skipped if anything was sent this recently: the radio was woken for
/// it then, and back-to-back bursts (seeks, track changes) shouldn't each cost a request.
pub const OPPORTUNISTIC_GAP: Duration = Duration::from_secs(30);

/// A pause shorter than this continues the same session on resume; a longer one starts a new
/// one. Without it, headset pauses, unplugs and phone calls would each leave a sliver of a
/// session in the server's history (and the server deletes those under 3 s anyway).
pub const SESSION_CONTINUE_WITHIN: Duration = Duration::from_secs(10 * 60);

/// The most listening one observation may add: anything longer between two observations is a
/// stall, a suspend or a stuck pipeline, not listening.
const MAX_STEP: Duration = Duration::from_secs(1);

/// What the player is doing, as the recorder sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Playing {
    pub account_id: String,
    pub server_id: String,
    pub item_id: String,
    pub is_playing: bool,
    pub position: f64,
    pub duration: f64,
    pub play_method: i64,
}

/// The clock, passed in so tests can drive it.
#[derive(Debug, Clone, Copy)]
pub struct Now {
    pub instant: Instant,
    /// Wall clock, milliseconds.
    pub ms: i64,
    pub date: chrono::NaiveDate,
}

impl Now {
    pub fn current() -> Self {
        let local = chrono::Local::now();
        Self { instant: Instant::now(), ms: local.timestamp_millis(), date: local.date_naive() }
    }
}

struct Open {
    row: ListeningSession,
    /// The last observation while playing: when, and at what position.
    last_step: Option<(Instant, f64)>,
    /// When playback paused, if it is paused.
    paused_at: Option<(Instant, chrono::NaiveDate)>,
}

impl Open {
    /// Adds the time since the last observation while playing, if the position moved on.
    fn count_step(&mut self, now: Now, position: f64) {
        let Some((at, from)) = self.last_step else { return };
        // A seek forward also advances the position; the step cap keeps it from counting as more
        // than the time that passed.
        if position > from {
            self.row.time_listening += now.instant.saturating_duration_since(at).min(MAX_STEP).as_secs_f64();
        }
    }
}

/// Turns the player's state into listening sessions. Fed every published player state (each tick
/// while playing, and every play/pause/seek), it opens a session when playback starts, counts the
/// time actually listened — only while the position advances, at most [`MAX_STEP`] per step — and
/// starts a new session after a long pause, on another day, or for another book.
#[derive(Default)]
pub struct SessionRecorder {
    open: Option<Open>,
}

impl SessionRecorder {
    /// Observes the player. Returns a session that just ended, to be saved closed.
    pub fn observe(&mut self, now: Now, playing: Option<&Playing>) -> Option<ListeningSession> {
        let Some(playing) = playing else { return self.close(now) };
        let same_book = self.open.as_ref().is_some_and(|open| {
            open.row.item_id == playing.item_id && open.row.account_id == playing.account_id && open.row.server_id == playing.server_id
        });
        let mut ended = if same_book { None } else { self.close(now) };

        if playing.is_playing {
            let stale = self.open.as_ref().and_then(|open| open.paused_at).is_some_and(|(at, date)| {
                now.instant.saturating_duration_since(at) >= SESSION_CONTINUE_WITHIN || date != now.date
            });
            if stale {
                ended = self.close(now).or(ended);
            }
            let open = self.open.get_or_insert_with(|| Open {
                row: ListeningSession {
                    id: abs_core::listening_sessions::new_session_id(),
                    account_id: playing.account_id.clone(),
                    server_id: playing.server_id.clone(),
                    item_id: playing.item_id.clone(),
                    play_method: playing.play_method,
                    start_position: playing.position,
                    current_position: playing.position,
                    time_listening: 0.0,
                    duration: playing.duration,
                    started_at: now.ms,
                    updated_at: now.ms,
                    sent_updated_at: None,
                    closed: false,
                },
                last_step: None,
                paused_at: None,
            });
            open.count_step(now, playing.position);
            open.last_step = Some((now.instant, playing.position));
            open.paused_at = None;
        } else if let Some(open) = &mut self.open {
            // What played between the last observation and the pause.
            open.count_step(now, playing.position);
            if open.paused_at.is_none() {
                open.paused_at = Some((now.instant, now.date));
            }
            open.last_step = None;
        }
        if let Some(open) = &mut self.open {
            open.row.current_position = playing.position;
            open.row.play_method = playing.play_method;
            if playing.duration > 0.0 {
                open.row.duration = playing.duration;
            }
        }
        ended
    }

    /// The open session as of `now`, to be saved alongside the progress, if it belongs to this
    /// book.
    pub fn current(&self, now: Now, item_id: &str) -> Option<ListeningSession> {
        let open = self.open.as_ref().filter(|open| open.row.item_id == item_id)?;
        Some(ListeningSession { updated_at: now.ms, ..open.row.clone() })
    }

    /// Ends the open session, if any, returning it closed.
    pub fn close(&mut self, now: Now) -> Option<ListeningSession> {
        let open = self.open.take()?;
        Some(ListeningSession { updated_at: now.ms, closed: true, ..open.row })
    }
}

/// How urgently a send is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendReason {
    /// Periodic, or a moment that matters (pause, stop, start, the network back).
    Due,
    /// The radio is awake anyway (a burst finished, another sync just ran): only if nothing was
    /// sent in the last [`OPPORTUNISTIC_GAP`].
    Opportunistic,
}

type Listener = Rc<dyn Fn(ProgressSyncOutcome)>;

#[derive(Default)]
struct SenderState {
    running: bool,
    /// A send asked for while one was running: done right after it, once.
    again: Option<(SqlitePool, abs_core::auth::Session, Option<Listener>)>,
    backoff: Backoff,
    last_send: Option<Instant>,
    /// Whether the current run of failures (or of offline skips) has been logged already.
    failure_logged: bool,
    offline_logged: bool,
}

/// Sends the listening sessions waiting on this device, one batch at a time, never two at once.
/// Asking again while a send runs queues exactly one more. Failures back off (see [`Backoff`])
/// instead of retrying on every tick.
#[derive(Clone, Default)]
pub struct SessionSender {
    state: Rc<RefCell<SenderState>>,
}

impl SessionSender {
    /// When something was last sent (or tried), for the periodic schedule.
    pub fn last_send(&self) -> Option<Instant> {
        self.state.borrow().last_send
    }

    pub fn is_running(&self) -> bool {
        self.state.borrow().running
    }

    /// Forgets the backoff — the network came back or offline mode was turned off, so the
    /// reason sends were failing may be gone.
    pub fn reset_backoff(&self) {
        let mut state = self.state.borrow_mut();
        state.backoff.reset();
    }

    /// Asks for a send of `session`'s account's waiting sessions.
    pub fn request(&self, pool: SqlitePool, session: abs_core::auth::Session, reason: SendReason, listener: Option<Listener>) {
        let now = Instant::now();
        {
            let mut state = self.state.borrow_mut();
            if session.is_offline() {
                if !state.offline_logged {
                    state.offline_logged = true;
                    tracing::info!("offline mode is on; listening sessions stay on the device until it's turned off");
                }
                return;
            }
            if !state.backoff.allows(now) {
                return;
            }
            if reason == SendReason::Opportunistic && state.last_send.is_some_and(|at| now.saturating_duration_since(at) < OPPORTUNISTIC_GAP) {
                return;
            }
            if state.running {
                state.again = Some((pool, session, listener));
                return;
            }
            state.running = true;
        }
        let sender = self.clone();
        glib::spawn_future_local(async move {
            let mut next = Some((pool, session, listener));
            while let Some((pool, session, listener)) = next.take() {
                sender.send_once(&pool, &session, listener.as_ref()).await;
                next = sender.state.borrow_mut().again.take();
            }
            sender.state.borrow_mut().running = false;
        });
    }

    async fn send_once(&self, pool: &SqlitePool, session: &abs_core::auth::Session, listener: Option<&Listener>) {
        // Nothing waiting: no request, and no token refresh or reachability probe either.
        match abs_storage::repo::listening_sessions::count_dirty(pool, session.account_id(), session.server_id()).await {
            Ok(0) => return,
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(%err, "couldn't read the listening sessions waiting to be sent");
                return;
            }
        }
        let _awake = session.keep_awake_for("Sending listening history to the server");
        let report = |outcome| {
            if let Some(listener) = listener {
                listener(outcome);
            }
        };
        let result = async {
            let api = session.api_client().await?;
            let device = abs_core::listening_sessions::device_info(pool).await?;
            abs_core::listening_sessions::send_pending(pool, &api, &device, session.account_id(), session.server_id()).await
        }
        .await;
        let mut state = self.state.borrow_mut();
        match result {
            Ok(_) => {
                state.last_send = Some(Instant::now());
                state.backoff.reset();
                if state.failure_logged || state.offline_logged {
                    tracing::info!("listening sessions reach the server again");
                }
                state.failure_logged = false;
                state.offline_logged = false;
                drop(state);
                report(ProgressSyncOutcome::Synced);
            }
            // Turned on between the request and now: kept for when it's turned off.
            Err(abs_core::CoreError::Offline) => {}
            Err(err) => {
                state.last_send = Some(Instant::now());
                let wait = state.backoff.failed(Instant::now());
                if state.failure_logged {
                    tracing::debug!(%err, retry_in_s = wait.as_secs(), "still couldn't send listening sessions");
                } else {
                    state.failure_logged = true;
                    tracing::warn!(%err, retry_in_s = wait.as_secs(), "couldn't send listening sessions to the server; they stay on the device");
                }
                drop(state);
                report(match err {
                    abs_core::CoreError::Auth => ProgressSyncOutcome::SessionExpired,
                    _ => ProgressSyncOutcome::Failed,
                });
            }
        }
    }
}

/// Saves a session row, logging rather than failing: losing one save loses seconds of history,
/// never playback.
pub async fn save(pool: &SqlitePool, row: &ListeningSession) {
    if let Err(err) = abs_storage::repo::listening_sessions::save(pool, row).await {
        tracing::warn!(%err, "couldn't save the listening session");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, seconds: f64) -> Now {
        Now {
            instant: start + Duration::from_secs_f64(seconds),
            ms: 1_000_000 + (seconds * 1000.0) as i64,
            date: chrono::NaiveDate::from_ymd_opt(2026, 10, 17).unwrap(),
        }
    }

    fn playing(item_id: &str, is_playing: bool, position: f64) -> Playing {
        Playing {
            account_id: "acc".into(),
            server_id: "srv".into(),
            item_id: item_id.into(),
            is_playing,
            position,
            duration: 3600.0,
            play_method: 3,
        }
    }

    #[test]
    fn listening_is_counted_only_while_the_position_advances_and_at_most_a_second_a_step() {
        let start = Instant::now();
        let mut recorder = SessionRecorder::default();
        assert!(recorder.current(at(start, 0.0), "book").is_none(), "nothing played yet");

        recorder.observe(at(start, 0.0), Some(&playing("book", true, 100.0)));
        for step in 1..=4 {
            recorder.observe(at(start, step as f64 * 0.25), Some(&playing("book", true, 100.0 + step as f64 * 0.25)));
        }
        let session = recorder.current(at(start, 1.0), "book").unwrap();
        assert!((session.time_listening - 1.0).abs() < 1e-9, "{}", session.time_listening);
        assert_eq!((session.start_position, session.current_position), (100.0, 101.0));

        // Buffering: playing, but the position doesn't move — not listening.
        recorder.observe(at(start, 3.0), Some(&playing("book", true, 101.0)));
        // A 30 s gap between observations (a suspend) counts as one step at most.
        recorder.observe(at(start, 33.0), Some(&playing("book", true, 101.5)));
        let session = recorder.current(at(start, 33.0), "book").unwrap();
        assert!((session.time_listening - 2.0).abs() < 1e-9, "{}", session.time_listening);
        assert!(recorder.current(at(start, 33.0), "another book").is_none());
    }

    #[test]
    fn a_short_pause_continues_the_session_and_a_long_one_starts_a_new_one() {
        let start = Instant::now();
        let mut recorder = SessionRecorder::default();
        recorder.observe(at(start, 0.0), Some(&playing("book", true, 0.0)));
        let first = recorder.current(at(start, 0.0), "book").unwrap().id;

        recorder.observe(at(start, 10.0), Some(&playing("book", false, 10.0)));
        assert!(recorder.observe(at(start, 70.0), Some(&playing("book", true, 10.0))).is_none());
        assert_eq!(recorder.current(at(start, 70.0), "book").unwrap().id, first, "paused a minute: same session");

        recorder.observe(at(start, 80.0), Some(&playing("book", false, 20.0)));
        let ended = recorder.observe(at(start, 80.0 + 11.0 * 60.0), Some(&playing("book", true, 20.0))).expect("the old session ends");
        assert_eq!(ended.id, first);
        assert!(ended.closed);
        assert_ne!(recorder.current(at(start, 800.0), "book").unwrap().id, first, "paused 11 minutes: a new session");
    }

    #[test]
    fn a_new_day_another_book_or_nothing_loaded_ends_the_session() {
        let start = Instant::now();
        let mut recorder = SessionRecorder::default();
        recorder.observe(at(start, 0.0), Some(&playing("book", true, 0.0)));
        recorder.observe(at(start, 5.0), Some(&playing("book", false, 5.0)));
        let mut tomorrow = at(start, 60.0);
        tomorrow.date = tomorrow.date.succ_opt().unwrap();
        assert!(recorder.observe(tomorrow, Some(&playing("book", true, 5.0))).is_some(), "past midnight: a new session");

        let ended = recorder.observe(at(start, 70.0), Some(&playing("other", true, 0.0))).expect("switching books ends it");
        assert_eq!(ended.item_id, "book");
        assert_eq!(recorder.current(at(start, 70.0), "other").unwrap().start_position, 0.0);

        assert_eq!(recorder.observe(at(start, 80.0), None).unwrap().item_id, "other");
        assert!(recorder.observe(at(start, 90.0), None).is_none());
    }

    #[test]
    fn nothing_is_recorded_until_playback_actually_starts() {
        let start = Instant::now();
        let mut recorder = SessionRecorder::default();
        // Loaded and seeked while paused: no session.
        recorder.observe(at(start, 0.0), Some(&playing("book", false, 0.0)));
        recorder.observe(at(start, 1.0), Some(&playing("book", false, 300.0)));
        assert!(recorder.current(at(start, 1.0), "book").is_none());

        recorder.observe(at(start, 2.0), Some(&playing("book", true, 300.0)));
        // A seek while paused moves the session's position, without counting as listening.
        recorder.observe(at(start, 3.0), Some(&playing("book", false, 301.0)));
        recorder.observe(at(start, 4.0), Some(&playing("book", false, 900.0)));
        let session = recorder.current(at(start, 4.0), "book").unwrap();
        assert_eq!(session.current_position, 900.0);
        assert!((session.time_listening - 1.0).abs() < 1e-9);
    }
}
