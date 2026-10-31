//! Resolves the active server's host name(s) at startup, so the system's resolver cache is warm
//! by the time the first request needs it.
//!
//! Nothing waits on this: it runs on the runtime in the background, a failure is only logged, and
//! in offline mode it does nothing at all. The lookup goes through the same `getaddrinfo` the HTTP
//! client uses, so what it warms is the system's cache (systemd-resolved, NetworkManager's dnsmasq),
//! not anything inside this process. The log lines carry no host names, which keeps anonymised
//! logs anonymous.

use std::future::Future;
use std::time::{Duration, Instant};

use sqlx::SqlitePool;

/// How long one lookup may take before it is given up on (and logged as such).
const LOOKUP_CAP: Duration = Duration::from_secs(5);

/// Which of the server's addresses a lookup is for (the label in the log line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Remote,
    Local,
}

impl Target {
    fn label(self) -> &'static str {
        match self {
            Target::Remote => "remote",
            Target::Local => "local",
        }
    }
}

/// The host and port to resolve for an address as the user typed it or the database holds it, or
/// `None` when there is nothing to resolve: blank, unparseable, no host, or an IP literal.
pub fn lookup_target(address: &str) -> Option<(String, u16)> {
    let address = address.trim();
    if address.is_empty() {
        return None;
    }
    let with_scheme = if address.contains("://") { address.to_string() } else { format!("http://{address}") };
    let url = reqwest::Url::parse(&with_scheme).ok()?;
    let port = url.port_or_known_default()?;
    let host = url.host_str()?;
    // An IPv6 literal comes bracketed.
    if host.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    Some((host.to_string(), port))
}

/// Looks up the active server's addresses in the background-friendly way described above. Reads
/// the server row and the offline-mode setting; a database error ends it quietly.
pub async fn warm_dns(pool: &SqlitePool, server_id: &str) {
    warm_dns_with(pool, server_id, |host, port| async move {
        tokio::net::lookup_host((host.as_str(), port)).await.map(|addresses| addresses.count()).map_err(|err| err.to_string())
    })
    .await;
}

/// [`warm_dns`] with the resolver passed in, for the tests.
pub async fn warm_dns_with<R, F>(pool: &SqlitePool, server_id: &str, resolve: R)
where
    R: Fn(String, u16) -> F + Clone,
    F: Future<Output = Result<usize, String>>,
{
    warm_dns_capped(pool, server_id, resolve, LOOKUP_CAP).await;
}

async fn warm_dns_capped<R, F>(pool: &SqlitePool, server_id: &str, resolve: R, cap: Duration)
where
    R: Fn(String, u16) -> F + Clone,
    F: Future<Output = Result<usize, String>>,
{
    if abs_storage_offline(pool).await {
        return;
    }
    let Ok(server) = abs_storage::repo::servers::get(pool, server_id).await else { return };
    let mut targets: Vec<(Target, String, u16)> = Vec::new();
    for (target, address) in [(Target::Remote, Some(server.url.as_str())), (Target::Local, server.local_network_address.as_deref())] {
        let Some((host, port)) = address.and_then(lookup_target) else { continue };
        // The same name for both addresses (a different port, say) needs one lookup.
        if targets.iter().any(|(_, known, _)| known.eq_ignore_ascii_case(&host)) {
            continue;
        }
        targets.push((target, host, port));
    }
    let lookups = targets.into_iter().map(|(target, host, port)| {
        let resolve = resolve.clone();
        async move {
            let started = Instant::now();
            let outcome = match tokio::time::timeout(cap, resolve(host, port)).await {
                Ok(outcome) => outcome,
                Err(_) => Err(format!("no answer within {}ms", cap.as_millis())),
            };
            let took_ms = started.elapsed().as_millis() as u64;
            match outcome {
                Ok(addresses) => tracing::info!(target = target.label(), took_ms, addresses, "dns warm-up"),
                Err(reason) => tracing::info!(target = target.label(), took_ms, %reason, "dns warm-up failed"),
            }
        }
    });
    futures_join_all(lookups).await;
}

async fn abs_storage_offline(pool: &SqlitePool) -> bool {
    crate::settings::load_offline_mode(pool).await.unwrap_or(false)
}

/// Runs the lookups side by side without pulling in a combinator crate for two futures.
async fn futures_join_all<F: Future<Output = ()>>(lookups: impl Iterator<Item = F>) {
    let mut set = std::pin::pin!(lookups.map(Box::pin).collect::<Vec<_>>());
    std::future::poll_fn(|cx| {
        set.retain_mut(|lookup| lookup.as_mut().poll(cx).is_pending());
        if set.is_empty() { std::task::Poll::Ready(()) } else { std::task::Poll::Pending }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn lookup_targets_come_from_urls_and_bare_addresses() {
        let some = |host: &str, port: u16| Some((host.to_string(), port));
        assert_eq!(lookup_target("https://abs.example.org"), some("abs.example.org", 443));
        assert_eq!(lookup_target("http://abs.example.org"), some("abs.example.org", 80));
        assert_eq!(lookup_target("https://abs.example.org:13378/audiobookshelf"), some("abs.example.org", 13378));
        assert_eq!(lookup_target("  abs.local:13378 "), some("abs.local", 13378), "no scheme: http is assumed");
        assert_eq!(lookup_target("abs.local"), some("abs.local", 80));
    }

    #[test]
    fn nothing_to_look_up_for_blank_addresses_or_ip_literals() {
        for address in ["", "   ", "http://192.168.1.20:13378", "192.168.1.20", "http://[::1]:13378", "http://", "ht!tp://%%"] {
            assert_eq!(lookup_target(address), None, "{address:?}");
        }
    }

    async fn pool_with_server(url: &str, local: Option<&str>) -> (SqlitePool, String) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = abs_storage::connect_and_migrate(&tmp.path().join("db.sqlite3")).await.unwrap();
        std::mem::forget(tmp);
        let id = abs_storage::repo::servers::add(&pool, url).await.unwrap();
        if let Some(local) = local {
            abs_storage::repo::servers::set_local_network_address(&pool, &id, Some(local)).await.unwrap();
        }
        (pool, id)
    }

    fn recording() -> (Arc<Mutex<Vec<(String, u16)>>>, impl Fn(String, u16) -> std::future::Ready<Result<usize, String>> + Clone) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_for_resolver = seen.clone();
        (seen, move |host, port| {
            seen_for_resolver.lock().unwrap().push((host, port));
            std::future::ready(Ok(2))
        })
    }

    #[tokio::test]
    async fn looks_up_the_remote_and_the_local_address() {
        let (pool, id) = pool_with_server("https://abs.example.org", Some("http://abs.lan:13378")).await;
        let (seen, resolver) = recording();
        warm_dns_with(&pool, &id, resolver).await;
        let mut seen = seen.lock().unwrap().clone();
        seen.sort();
        assert_eq!(seen, vec![("abs.example.org".to_string(), 443), ("abs.lan".to_string(), 13378)]);
    }

    #[tokio::test]
    async fn one_name_is_looked_up_once_and_ip_literals_not_at_all() {
        let (pool, id) = pool_with_server("https://abs.example.org", Some("http://ABS.example.org:13378")).await;
        let (seen, resolver) = recording();
        warm_dns_with(&pool, &id, resolver).await;
        assert_eq!(seen.lock().unwrap().len(), 1);

        let (pool, id) = pool_with_server("http://192.168.1.20:13378", None).await;
        let (seen, resolver) = recording();
        warm_dns_with(&pool, &id, resolver).await;
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn offline_mode_looks_nothing_up() {
        let (pool, id) = pool_with_server("https://abs.example.org", Some("http://abs.lan")).await;
        crate::settings::save_offline_mode(&pool, true).await.unwrap();
        let (seen, resolver) = recording();
        warm_dns_with(&pool, &id, resolver).await;
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failing_or_missing_server_is_not_an_error() {
        let (pool, id) = pool_with_server("https://abs.example.org", None).await;
        warm_dns_with(&pool, &id, |_, _| std::future::ready(Err::<usize, String>("no such host".into()))).await;
        warm_dns_with(&pool, "no-such-server", |_, _| std::future::ready(Ok(1))).await;
    }

    #[tokio::test]
    async fn a_lookup_that_never_answers_is_cut_off() {
        let (pool, id) = pool_with_server("https://abs.example.org", Some("http://abs.lan")).await;
        let started = Instant::now();
        warm_dns_capped(&pool, &id, |_, _| std::future::pending::<Result<usize, String>>(), Duration::from_millis(100)).await;
        let took = started.elapsed();
        // Both lookups hang; they run side by side, so the cap is paid once, not twice.
        assert!(took >= Duration::from_millis(100) && took < Duration::from_millis(190), "{took:?}");
    }

    #[tokio::test]
    async fn the_log_lines_carry_no_host_name() {
        use std::io::Write;
        #[derive(Clone, Default)]
        struct Buffer(Arc<Mutex<Vec<u8>>>);
        impl Write for Buffer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt().with_writer(move || writer.clone()).with_ansi(false).finish();
        let (pool, id) = pool_with_server("https://secret-host.example.org", Some("http://secret-lan:13378")).await;
        let guard = tracing::subscriber::set_default(subscriber);
        warm_dns_with(&pool, &id, |_, _| std::future::ready(Ok::<usize, String>(1))).await;
        warm_dns_with(&pool, &id, |_, _| std::future::ready(Err::<usize, String>("x".into()))).await;
        drop(guard);
        let logged = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        assert!(logged.contains("dns warm-up"), "{logged}");
        assert!(logged.contains("remote") && logged.contains("local"), "{logged}");
        assert!(!logged.contains("secret"), "{logged}");
    }
}
