//! Local-only (never phones home) crash observability: a persistent, rotating log file under
//! `AppPaths::logs_dir()`, a panic hook that always captures a full backtrace (no
//! `RUST_BACKTRACE` needed), and local Breakpad-format `.dmp` files for native (signal-level)
//! crashes — a segfault/abort inside the GTK/GStreamer/glib C code this binary links against,
//! which a Rust panic hook can never catch. All of this exists because a deployed build normally
//! has no terminal attached (a phone running Phosh) — without it, a crash today leaves no trace a
//! user could ever hand back in a bug report.
//!
//! Ordinary logging deliberately avoids touching flash on every event: writes accumulate in an
//! in-memory buffer and only reach disk on a coarse periodic timer, on a panic, or on clean
//! shutdown — see [`BufferedLogWriter`].
//!
//! Native crash dumps use an out-of-process design (`crash-handler` + `minidumper`, Embark
//! Studios; local IPC + local file only, never networked): this binary re-execs itself as a tiny
//! crash-server subprocess (see [`crash_server_socket_name`]/[`run_crash_server`]) that receives
//! a crash report over a Unix domain socket and writes the `.dmp` file — safer than writing one
//! from inside the crashing process's own signal handler, where very few operations are actually
//! safe to perform.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// A `Write` implementation that buffers in memory (via `BufWriter`) and only touches the
/// underlying rotating log file when that buffer fills or [`flush`](Self::flush) is called
/// explicitly — see the module doc comment for why. `Clone` is cheap (an `Arc` bump); every clone
/// shares the same underlying buffer/file.
#[derive(Clone)]
struct BufferedLogWriter {
    inner: Arc<Mutex<std::io::BufWriter<RollingFileAppender>>>,
    /// Set on every `write`, cleared by `flush` — lets the periodic flush thread skip the
    /// syscall (and the mutex lock) entirely on a tick where nothing was logged, rather than
    /// waking every minute to flush zero bytes for as long as the app sits idle.
    dirty: Arc<AtomicBool>,
}

/// Ordinary logging traffic over a minute of normal use shouldn't approach this before the
/// periodic timer flushes it anyway; sized generously so a burst of activity still doesn't
/// trigger a mid-buffer disk write.
const LOG_BUFFER_CAPACITY: usize = 64 * 1024;

/// How often the background timer flushes buffered log lines to disk during ordinary operation.
/// Trade-off, stated plainly: a hard kill (`SIGKILL`, a native crash) can lose up to this much of
/// the most recent log output — acceptable because a native crash's authoritative artifact is the
/// `.dmp` file crash dumps write (not this log), and a panic flushes immediately regardless of
/// this timer (see [`install_panic_hook`]).
const PERIODIC_FLUSH_INTERVAL: Duration = Duration::from_secs(60);

impl BufferedLogWriter {
    fn new(appender: RollingFileAppender) -> Self {
        Self {
            inner: Arc::new(Mutex::new(std::io::BufWriter::with_capacity(LOG_BUFFER_CAPACITY, appender))),
            dirty: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Flushes only if something was written since the last flush — used both by the periodic
    /// timer (so an app sitting idle doesn't lock the mutex and hit the OS every minute for zero
    /// bytes) and by [`install_panic_hook`]'s hook: a panic always logs its own backtrace
    /// immediately beforehand, which is what actually needs flushing, so `dirty` is already true
    /// by the time that call lands here — this never skips a flush a panic depends on.
    fn flush(&self) {
        if self.dirty.swap(false, Ordering::Relaxed) {
            if let Ok(mut writer) = self.inner.lock() {
                let _ = writer.flush();
            }
        }
    }
}

impl Write for BufferedLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.lock().expect("log writer mutex").write(buf)?;
        if written > 0 {
            self.dirty.store(true, Ordering::Relaxed);
        }
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.lock().expect("log writer mutex").flush()
    }
}

/// The process-wide log writer, so UI code can force pending log lines to disk (see
/// [`flush_logs`]) without being handed the writer itself.
static LOG_WRITER: std::sync::OnceLock<BufferedLogWriter> = std::sync::OnceLock::new();

/// Writes any buffered log lines to disk now. Logging buffers for up to a minute (see
/// [`PERIODIC_FLUSH_INTERVAL`]), so anything about to hand the log file to the user — "open the
/// log" — flushes first. A no-op before [`init_logging`] has run.
/// Where this run's log files are, once logging is up.
static LOG_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn log_dir() -> Option<PathBuf> {
    LOG_DIR.get().cloned()
}

pub fn flush_logs() {
    if let Some(writer) = LOG_WRITER.get() {
        writer.flush();
    }
}

/// Held for the lifetime of `main()`. Dropping this has no special behavior (the buffered writer
/// is happy to just stop being flushed); callers explicitly call [`flush`](Self::flush) at the
/// moments that matter (panic, clean shutdown) rather than relying on `Drop` timing.
#[derive(Clone)]
pub struct LogHandle {
    writer: BufferedLogWriter,
}

impl LogHandle {
    pub fn flush(&self) {
        self.writer.flush();
    }
}

/// The subscriber `init_logging` installs, with both sinks wrapped in the log-privacy scrubber
/// (see `crate::log_privacy`). Split out so tests can drive the real pipeline into in-memory
/// sinks.
pub(crate) fn build_subscriber<F, O>(
    filter: tracing_subscriber::EnvFilter,
    file_sink: F,
    stdout_sink: O,
    redactor: crate::log_privacy::LogRedactor,
) -> impl tracing::Subscriber + Send + Sync + 'static
where
    F: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
    O: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    use crate::log_privacy::RedactingMakeWriter;
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(RedactingMakeWriter::new(file_sink, redactor.clone()));
    let stdout_layer = tracing_subscriber::fmt::layer().with_writer(RedactingMakeWriter::new(stdout_sink, redactor));
    tracing_subscriber::registry().with(filter).with(file_layer).with(stdout_layer)
}

/// Installs the global `tracing` subscriber: a rotating file layer under `paths.logs_dir()`
/// (human-readable, no ANSI color codes — a user attaches this file to a bug report, nobody
/// pipes it through a colorizer) plus an unconditional stdout layer (so `cargo run`'s dev
/// experience is unchanged), both passing through the privacy scrubber, and both filtered to `info` by default (`RUST_LOG` overrides). Starts a
/// detached background thread that flushes the file layer's buffer every
/// [`PERIODIC_FLUSH_INTERVAL`] — see the module doc comment.
///
/// Must be called before anything else logs (including [`install_panic_hook`]'s hook firing) and
/// after `paths.logs_dir()` exists on disk (`AppPaths::ensure_early_dirs`).
pub fn init_logging(paths: &abs_storage::AppPaths) -> LogHandle {
    let appender = |dir: &std::path::Path| {
        RollingFileAppender::builder().rotation(Rotation::DAILY).filename_prefix(abs_storage::paths::LOG_FILE_PREFIX).max_log_files(7).build(dir)
    };
    // The app's own log folder, or — when it can't be written (a full disk, a read-only home)
    // — a temporary one, so the startup problem that follows is still logged and shown.
    let fallback = std::env::temp_dir().join("audiobooklet-logs");
    let (appender, dir) = match appender(&paths.logs_dir()) {
        Ok(appender) => (appender, paths.logs_dir()),
        Err(err) => {
            eprintln!("audiobooklet: can't write logs to {}: {err}; using {}", paths.logs_dir().display(), fallback.display());
            let _ = std::fs::create_dir_all(&fallback);
            (appender(&fallback).expect("build the rotating log file appender"), fallback)
        }
    };
    let _ = LOG_DIR.set(dir);

    let writer = BufferedLogWriter::new(appender);
    let _ = LOG_WRITER.set(writer.clone());

    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let file_writer = {
        let writer = writer.clone();
        move || writer.clone()
    };
    build_subscriber(filter, file_writer, std::io::stdout, crate::log_privacy::global().clone()).init();

    {
        let writer = writer.clone();
        std::thread::Builder::new()
            .name("log-flush".to_string())
            .spawn(move || loop {
                std::thread::sleep(PERIODIC_FLUSH_INTERVAL);
                writer.flush();
            })
            .expect("spawn the periodic log-flush thread");
    }

    LogHandle { writer }
}

/// Replaces the default panic hook with one that always captures a full backtrace
/// (`std::backtrace::Backtrace::force_capture` ignores `RUST_BACKTRACE` entirely, unlike the
/// default hook) and logs it via `tracing::error!` — reaching the log file from [`init_logging`]
/// even with no terminal attached — then flushes immediately (a panic is exactly the moment this
/// data matters most; it must not wait for the periodic timer). The previous hook still runs
/// afterwards, so default terminal output on an attached-terminal desktop run is unchanged, as is
/// normal unwind/abort behavior (this hook only adds a logging side effect).
pub fn install_panic_hook(log_handle: LogHandle) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(%backtrace, "panic: {info}");
        log_handle.flush();
        previous(info);
    }));
}

const CRASH_SERVER_FLAG_PREFIX: &str = "--crash-handler-server=";

/// If the process was invoked with the hidden crash-server flag, returns the socket name it
/// should serve on. `main()` checks this before anything else (even `--version`) — a real user
/// never passes this flag; it exists only so this binary can re-exec itself as the crash-dump
/// server (see the module doc comment).
pub fn crash_server_socket_name() -> Option<String> {
    std::env::args().find_map(|arg| arg.strip_prefix(CRASH_SERVER_FLAG_PREFIX).map(str::to_string))
}

/// Runs this process as the crash-dump server and never returns (calls `std::process::exit` once
/// the server's message loop ends). Must be called before any other startup side effect — no
/// database, no GStreamer, no GTK application — since this is purely an internal IPC responder,
/// never a real app instance a user interacts with.
pub fn run_crash_server(socket_name: &str, crash_dumps_dir: PathBuf) -> ! {
    struct Handler {
        crash_dumps_dir: PathBuf,
    }

    impl minidumper::ServerHandler for Handler {
        fn create_minidump_file(&self) -> Result<(std::fs::File, PathBuf), std::io::Error> {
            let _ = std::fs::create_dir_all(&self.crash_dumps_dir);
            let pid = std::process::id();
            let unix_secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
            let path = self.crash_dumps_dir.join(format!("crash-{unix_secs}-{pid}.dmp"));
            let file = std::fs::File::create(&path)?;
            Ok((file, path))
        }

        fn on_minidump_created(&self, result: Result<minidumper::MinidumpBinary, minidumper::Error>) -> minidumper::LoopAction {
            match result {
                Ok(mut minidump) => {
                    let _ = minidump.file.flush();
                    tracing::info!(path = ?minidump.path, "wrote a native-crash minidump");
                }
                Err(err) => tracing::warn!(%err, "failed to write a native-crash minidump"),
            }
            minidumper::LoopAction::Exit
        }

        fn on_message(&self, _kind: u32, _buffer: Vec<u8>) {}

        /// The main app process closes its end of the socket as soon as it exits, whether that's
        /// a normal quit or a hard `SIGKILL` — the OS closes file descriptors on process
        /// termination regardless of cause. Exiting here once the last client is gone is what
        /// prevents this server from outliving the app it's monitoring, with no separate
        /// "clean shutdown" message needed.
        fn on_client_disconnected(&self, num_clients: usize) -> minidumper::LoopAction {
            if num_clients == 0 { minidumper::LoopAction::Exit } else { minidumper::LoopAction::Continue }
        }
    }

    let socket = minidumper::SocketName::abstract_namespace(socket_name);
    let mut server = minidumper::Server::with_name(socket).expect("create the crash-dump IPC server");
    let shutdown = std::sync::atomic::AtomicBool::new(false);
    if let Err(err) = server.run(Box::new(Handler { crash_dumps_dir }), &shutdown, None) {
        tracing::warn!(%err, "crash-dump server exited with an error");
    }
    std::process::exit(0);
}

/// Held for the lifetime of `main()`'s body — dropping this detaches the signal handler and lets
/// the crash-server subprocess's socket see a disconnect (which makes it exit; see
/// `run_crash_server`'s `on_client_disconnected`).
pub struct ClientHandles {
    _handler: crash_handler::CrashHandler,
    _client: Arc<minidumper::Client>,
    /// Not otherwise touched — the server subprocess's exit is driven entirely by
    /// `run_crash_server`'s `on_client_disconnected`, triggered by `_client`'s socket closing
    /// whenever this process exits (cleanly or otherwise). Kept only so this handle, not a bare
    /// PID, is what represents "the server we spawned."
    _server_process: std::process::Child,
}

/// Spawns this same binary as a crash-dump server subprocess (which resolves its own `AppPaths`
/// for `crash_dumps_dir()` — nothing needs to be passed to it) and attaches a signal handler that
/// forwards native (SIGSEGV/SIGABRT/SIGBUS/SIGILL/SIGFPE) crashes to it. Any failure along the
/// way (subprocess spawn refused, socket connect timeout, signal handler attach refused — e.g.
/// under a sandboxed/seccomp environment) degrades to a warning and `None`, exactly like
/// `abs_player::init()`'s existing GStreamer-failure handling in `main.rs` — crash-dump capture
/// is never allowed to block the app from starting.
pub fn attach_crash_handler() -> Option<ClientHandles> {
    let socket_name = format!("audiobooklet-crash-{}", std::process::id());

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            tracing::warn!(%err, "crash-dump capture unavailable: couldn't resolve the running executable's path");
            return None;
        }
    };

    let mut server_process = match std::process::Command::new(&exe).arg(format!("{CRASH_SERVER_FLAG_PREFIX}{socket_name}")).spawn() {
        Ok(child) => child,
        Err(err) => {
            tracing::warn!(%err, "crash-dump capture unavailable: couldn't spawn the crash-dump server");
            return None;
        }
    };

    let socket = minidumper::SocketName::abstract_namespace(&socket_name);
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    let client = loop {
        match minidumper::Client::with_name(socket) {
            Ok(client) => break Some(client),
            Err(_) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Err(err) => {
                tracing::warn!(%err, "crash-dump capture unavailable: couldn't connect to the crash-dump server");
                break None;
            }
        }
    };
    let Some(client) = client else {
        let _ = server_process.kill();
        return None;
    };
    let client = Arc::new(client);

    let handler = {
        let client_for_crash = client.clone();
        #[allow(unsafe_code)]
        unsafe {
            crash_handler::CrashHandler::attach(crash_handler::make_crash_event(move |crash_context: &crash_handler::CrashContext| {
                let _ = client_for_crash.ping();
                crash_handler::CrashEventResult::Handled(client_for_crash.request_dump(crash_context).is_ok())
            }))
        }
    };
    let handler = match handler {
        Ok(handler) => handler,
        Err(err) => {
            tracing::warn!(%err, "crash-dump capture unavailable: couldn't attach the signal handler");
            let _ = server_process.kill();
            return None;
        }
    };

    // Restricts who may inspect this process for crash information to the server we just spawned
    // (Linux-only concept — matches the upstream `minidumper` example).
    handler.set_ptracer(Some(server_process.id()));

    Some(ClientHandles { _handler: handler, _client: client, _server_process: server_process })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::log_privacy::LogRedactor;

    #[test]
    fn flush_makes_buffered_log_lines_visible_on_disk() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let appender = tracing_appender::rolling::RollingFileAppender::builder()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix(abs_storage::paths::LOG_FILE_PREFIX)
            .build(dir.path())
            .unwrap();
        let mut writer = super::BufferedLogWriter::new(appender);
        writer.write_all(b"hello log\n").unwrap();

        let read = || std::fs::read_to_string(dir.path().read_dir().unwrap().next().unwrap().unwrap().path()).unwrap();
        assert_eq!(read(), "", "still sitting in the in-memory buffer");
        writer.flush();
        assert_eq!(read(), "hello log\n");
    }

    #[test]
    fn flush_logs_before_init_is_a_no_op() {
        super::flush_logs();
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Capture {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    /// Runs `emit` under the real subscriber with both sinks captured; returns (file, stdout).
    fn run(redactor: LogRedactor, emit: impl FnOnce()) -> (String, String) {
        let (file, stdout) = (Capture::default(), Capture::default());
        let subscriber = super::build_subscriber(
            tracing_subscriber::EnvFilter::new("debug"),
            {
                let file = file.clone();
                move || file.clone()
            },
            {
                let stdout = stdout.clone();
                move || stdout.clone()
            },
            redactor,
        );
        tracing::subscriber::with_default(subscriber, emit);
        (file.text(), stdout.text())
    }

    /// A real reqwest failure against a port nothing listens on: its Display carries the full
    /// request URL, exactly what `%err` puts into the log at ~20 call sites.
    fn real_reqwest_error(base: &str) -> reqwest::Error {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime
            .block_on(reqwest::Client::new().get(format!("{base}/api/items/abc/file/1?token=SECRET-TOKEN")).send())
            .expect_err("nothing listens there")
    }

    #[test]
    fn reqwest_errors_hide_the_server_address_in_both_sinks_when_anonymizing() {
        let base = "http://127.0.0.1:1";
        let err = real_reqwest_error(base);
        assert!(err.to_string().contains("127.0.0.1"), "premise: the raw error names the host: {err}");

        let redactor = LogRedactor::new();
        redactor.register_server_url(base);
        let (file, stdout) = run(redactor, || tracing::warn!(%err, "couldn't sync"));
        for out in [&file, &stdout] {
            assert!(out.contains("couldn't sync"), "the event itself still logs: {out}");
            assert!(!out.contains("127.0.0.1"), "server address leaked: {out}");
            assert!(!out.contains("SECRET-TOKEN"), "token leaked: {out}");
        }
    }

    #[test]
    fn nothing_is_scrubbed_when_anonymization_is_off() {
        let base = "http://127.0.0.1:1";
        let err = real_reqwest_error(base);
        let redactor = LogRedactor::new();
        redactor.register_server_url(base);
        redactor.set_enabled(false);
        let (file, stdout) = run(redactor, || tracing::warn!(%err, "couldn't sync"));
        assert!(file.contains("127.0.0.1:1") && stdout.contains("127.0.0.1:1"), "{file} / {stdout}");
    }

    #[test]
    fn third_party_targets_and_structured_fields_are_scrubbed_too() {
        let (file, _) = run(LogRedactor::new(), || {
            tracing::debug!(target: "hyper_util::client::legacy::connect::http", "connecting to http://abs.example.com:13378/ping");
            tracing::info!(url = "https://abs.example.com/x", "structured");
        });
        assert!(!file.contains("abs.example.com"), "{file}");
        assert!(file.contains("connecting to <url>") && file.contains("url=\"<url>\""), "{file}");
    }

    #[test]
    fn toggling_takes_effect_on_the_live_subscriber() {
        let redactor = LogRedactor::new();
        let handle = redactor.clone();
        let (file, _) = run(redactor, || {
            tracing::info!("one https://abs.example.com/a");
            handle.set_enabled(false);
            tracing::info!("two https://abs.example.com/b");
            handle.set_enabled(true);
            tracing::info!("three https://abs.example.com/c");
        });
        assert!(file.contains("one <url>"), "{file}");
        assert!(file.contains("two https://abs.example.com/b"), "{file}");
        assert!(file.contains("three <url>"), "{file}");
    }

    #[test]
    fn the_unconfigured_global_redactor_is_on() {
        assert!(crate::log_privacy::global().is_enabled());
    }
}
