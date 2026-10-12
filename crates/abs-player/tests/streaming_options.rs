//! What the playback engine (GStreamer's `souphttpsrc`, not the app's HTTP client) does with a
//! server's connection options, against `abs-core/tests/support/fake_abs.py` serving a real WAV
//! over plain HTTP, a self-signed certificate, a shared-secret header gate and mutual TLS.
//!
//! `#[ignore]`d because it needs `python3`, `openssl`, GStreamer's `souphttpsrc` and TLS support
//! (glib-networking). Run with:
//!   cargo test -p abs-player --test streaming_options -- --ignored --test-threads=1 --nocapture
//!
//! `playback_with_a_client_certificate_is_not_supported` pins a documented limitation: the
//! app's own HTTP client (API calls, downloads) can present a client certificate, but streaming
//! playback cannot, so a server that demands one only plays books that were downloaded first.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use abs_player::{AudioBackend, ConnectionProperties, GstBackend, PlayerEvent};

const RIG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../abs-core/tests/support/fake_abs.py");
const GATE: &str = "X-Proxy-Auth: s3cret";

struct Rig {
    child: Child,
    port: u16,
    tls: bool,
    _dir: tempfile::TempDir,
}

impl Rig {
    fn start(certs: Option<&std::path::Path>, mtls: bool, header: Option<&str>) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let mut command = Command::new("python3");
        command.args(["-I", RIG, "serve", "--log"]).arg(dir.path().join("requests.log"));
        if let Some(certs) = certs {
            command.arg("--tls").arg(certs);
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
        Rig { child, port, tls: certs.is_some(), _dir: dir }
    }

    fn track_url(&self) -> String {
        format!("{}://127.0.0.1:{}/api/items/item-1/file/ino-1?token=a.b.c", if self.tls { "https" } else { "http" }, self.port)
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn certs() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    assert!(Command::new("python3").args(["-I", RIG, "certs"]).arg(dir.path()).status().unwrap().success());
    dir
}

#[derive(Debug)]
enum Outcome {
    Played,
    Failed(#[allow(dead_code)] String),
    TimedOut,
}

fn try_playing(url: &str, properties: ConnectionProperties) -> Outcome {
    abs_player::init().expect("GStreamer");
    let mut backend = GstBackend::new_with_sink("fakesink").expect("GStreamer");
    backend.apply_connection(&properties);
    backend.load(url).unwrap();
    backend.play().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match backend.poll_event() {
            Some(PlayerEvent::Error(err)) => return Outcome::Failed(format!("{err:?}")),
            Some(PlayerEvent::EndOfStream) => return Outcome::Played,
            None => {}
        }
        if backend.position().is_some_and(|p| p > Duration::from_millis(300)) {
            return Outcome::Played;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Outcome::TimedOut
}

fn strict() -> ConnectionProperties {
    ConnectionProperties { ssl_strict: true, ..ConnectionProperties::default() }
}

fn header() -> Vec<(String, String)> {
    vec![("X-Proxy-Auth".to_string(), "s3cret".to_string())]
}

#[test]
#[ignore]
fn plain_http_plays() {
    let rig = Rig::start(None, false, None);
    let outcome = try_playing(&rig.track_url(), strict());
    eprintln!("plain http: {outcome:?}");
    assert!(matches!(outcome, Outcome::Played), "{outcome:?}");
}

#[test]
#[ignore]
fn a_self_signed_server_plays_only_with_verification_off() {
    let certs = certs();
    let rig = Rig::start(Some(certs.path()), false, None);
    let refused = try_playing(&rig.track_url(), strict());
    eprintln!("self-signed, strict: {refused:?}");
    assert!(matches!(refused, Outcome::Failed(_)), "{refused:?}");
    let allowed = try_playing(&rig.track_url(), ConnectionProperties { ssl_strict: false, ..ConnectionProperties::default() });
    eprintln!("self-signed, not strict: {allowed:?}");
    assert!(matches!(allowed, Outcome::Played), "{allowed:?}");
}

#[test]
#[ignore]
fn a_header_gated_server_plays_only_with_the_header() {
    let rig = Rig::start(None, false, Some(GATE));
    let without = try_playing(&rig.track_url(), strict());
    eprintln!("header gate, no header: {without:?}");
    assert!(matches!(without, Outcome::Failed(_)), "{without:?}");
    let with = try_playing(&rig.track_url(), ConnectionProperties { extra_headers: header(), ssl_strict: true, ..ConnectionProperties::default() });
    eprintln!("header gate, header: {with:?}");
    assert!(matches!(with, Outcome::Played), "{with:?}");
}

#[test]
#[ignore]
fn self_signed_plus_header_gate_plays_with_both_options() {
    let certs = certs();
    let rig = Rig::start(Some(certs.path()), false, Some(GATE));
    let outcome = try_playing(&rig.track_url(), ConnectionProperties { extra_headers: header(), ssl_strict: false, ..ConnectionProperties::default() });
    eprintln!("self-signed + header: {outcome:?}");
    assert!(matches!(outcome, Outcome::Played), "{outcome:?}");
}

#[test]
#[ignore]
fn playback_with_a_client_certificate_is_not_supported() {
    let certs = certs();
    let rig = Rig::start(Some(certs.path()), true, Some(GATE));
    let outcome = try_playing(&rig.track_url(), ConnectionProperties { extra_headers: header(), ssl_strict: false, ..ConnectionProperties::default() });
    eprintln!("mTLS (everything else configured): {outcome:?}");
    assert!(matches!(outcome, Outcome::Failed(_) | Outcome::TimedOut), "if this plays, client certificates now work for streaming: {outcome:?}");
}
