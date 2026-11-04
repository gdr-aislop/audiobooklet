//! Verifies `add_server_and_login` against a real, live Audiobookshelf instance rather than only
//! `wiremock` — the public demo server at https://audiobooks.dev/audiobookshelf (username and
//! password both `demo`). Confirmed reachable and running server 2.36.0 (matching what the
//! vendored spec and `abs_api::Client::login` were built against) via `GET
//! /audiobookshelf/status` before writing this test.
//!
//! Note: this server issues a unique per-session username on login (e.g. `demo467087`, not
//! literally `demo`) — discovered by actually running this test, presumably so each concurrent
//! demo visitor gets an isolated guest account. The assertions below account for that.
//!
//! `#[ignore]`d so `cargo test --workspace` stays hermetic — this depends on outside network and
//! a specific external service staying up. Run explicitly with:
//!   cargo test -p abs-core --test live_demo_server -- --ignored --nocapture

use abs_storage::repo::{accounts, servers};

const DEMO_SERVER_URL: &str = "https://audiobooks.dev/audiobookshelf";

#[tokio::test]
#[ignore]
async fn live_login_against_demo_server_succeeds() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let pool = abs_storage::connect_and_migrate(&tmp.path().join("db.sqlite3"))
        .await
        .expect("connect and migrate temp database");

    let added = abs_core::accounts::add_server_and_login(&pool, DEMO_SERVER_URL, "demo", "demo", &abs_core::passwords::NoPasswords, false)
        .await
        .expect("login against the live demo server should succeed");

    let stored_server = servers::get(&pool, &added.server_id).await.unwrap();
    assert_eq!(stored_server.url, DEMO_SERVER_URL);

    let stored_account = accounts::get(&pool, &added.account_id).await.unwrap();
    // This public demo server issues a unique per-session username (e.g. "demo467087") rather
    // than always "demo" literally — discovered by actually running this test, not assumed —
    // presumably to give each concurrent demo visitor an isolated guest account.
    assert!(
        stored_account.username.starts_with("demo"),
        "expected a demo-prefixed username, got {:?}",
        stored_account.username
    );
    assert!(
        !stored_account.token.is_empty(),
        "a real access token should have been persisted"
    );

    println!(
        "live login OK: server_id={} account_id={} token_len={}",
        added.server_id,
        added.account_id,
        stored_account.token.len()
    );
}

#[tokio::test]
#[ignore]
async fn live_login_against_demo_server_with_wrong_password_fails_cleanly() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let pool = abs_storage::connect_and_migrate(&tmp.path().join("db.sqlite3"))
        .await
        .expect("connect and migrate temp database");

    let result =
        abs_core::accounts::add_server_and_login(&pool, DEMO_SERVER_URL, "demo", "definitely-wrong", &abs_core::passwords::NoPasswords, false)
            .await;

    assert!(result.is_err(), "wrong credentials must not succeed");
    assert!(
        servers::list(&pool).await.unwrap().is_empty(),
        "no server row should remain after a failed live login"
    );
}

/// The whole sync against the real server, checking what the mocks can't: that the series list
/// really comes back and lands in the cache the Library groups by (the demo library has the
/// "Aesop's Fables" series, whose books' display names are "Aesop's Fables #1", "#2", ...). A
/// plain, unpaged `/series` request answers `"results": []` on this server, which no mock caught.
#[tokio::test]
#[ignore]
async fn live_sync_caches_the_series_with_clean_names_in_the_servers_order() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let pool = abs_storage::connect_and_migrate(&tmp.path().join("db.sqlite3")).await.expect("connect and migrate temp database");
    let added = abs_core::accounts::add_server_and_login(&pool, DEMO_SERVER_URL, "demo", "demo", &abs_core::passwords::NoPasswords, false)
        .await
        .expect("login against the live demo server should succeed");
    let token = accounts::get(&pool, &added.account_id).await.unwrap().token;
    let paths = abs_storage::AppPaths::rooted_at(tmp.path().join("data"), tmp.path().join("cache"), tmp.path().join("state"));

    abs_core::sync::sync_all(&pool, &abs_core::connection::ConnectionTarget::direct(DEMO_SERVER_URL), &paths, &added.server_id, &token)
        .await
        .expect("sync against the live demo server");

    let memberships = abs_storage::repo::series_books::list_for_server(&pool, &added.server_id).await.unwrap();
    println!("cached series rows: {memberships:#?}");
    let fables: Vec<_> = memberships.iter().filter(|m| m.series_name == "Aesop's Fables").collect();
    assert!(fables.len() >= 2, "the live series list should have cached the Aesop's Fables books: {memberships:?}");
    assert!(memberships.iter().all(|m| !m.series_name.contains('#')), "names are clean, no \"#N\"");

    // The books' titles say their number; the cache must hold them in that order.
    let mut items = Vec::new();
    for library in abs_storage::repo::libraries::list_for_server(&pool, &added.server_id).await.unwrap() {
        items.extend(abs_storage::repo::items::list_for_library(&pool, &added.server_id, &library.id).await.unwrap());
    }
    let number_of = |item_id: &str| -> u32 {
        let item = items.iter().find(|item| item.id == item_id).expect("a cached series book is a stored item");
        item.series_name.as_deref().and_then(|name| name.rsplit('#').next()).and_then(|n| n.trim().parse().ok()).expect("a display name ending in #N")
    };
    let numbers: Vec<u32> = fables.iter().map(|m| number_of(&m.item_id)).collect();
    let mut sorted = numbers.clone();
    sorted.sort_unstable();
    assert_eq!(numbers, sorted, "the books are cached in series order");

    // The real numbers, from the items filtered by the series, match what the titles say.
    let cached: Vec<Option<u32>> = fables.iter().map(|m| m.sequence.as_deref().and_then(|s| s.parse().ok())).collect();
    assert_eq!(cached, numbers.iter().map(|n| Some(*n)).collect::<Vec<_>>(), "each book is cached with its real number in the series");
}
