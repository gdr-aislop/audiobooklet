//! Listening sessions against a real Audiobookshelf server — what the mock-server tests can't
//! show: that the server stores them, counts them in its stats, applies (or refuses) their
//! position, and keeps one device record. `#[ignore]`d; run against a server you can write to:
//!
//! ```sh
//! ABS_TEST_URL=http://127.0.0.1:13378 ABS_TEST_TOKEN=<access token> ABS_TEST_ITEM=<library item id> \
//!   cargo test -p abs-core --test real_server_sessions -- --ignored --nocapture
//! ```
//!
//! It adds sessions to that user's listening history and moves the item's progress.

use abs_core::listening_sessions::{device_info, send_pending, PLAY_METHOD_LOCAL};
use abs_storage::models::ListeningSession;
use abs_storage::repo::{accounts, items, libraries, listening_sessions, servers};

struct Env {
    url: String,
    token: String,
    item_id: String,
}

fn env() -> Env {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
    Env { url: var("ABS_TEST_URL"), token: var("ABS_TEST_TOKEN"), item_id: var("ABS_TEST_ITEM") }
}

async fn get_json(env: &Env, path: &str) -> serde_json::Value {
    reqwest::Client::new()
        .get(format!("{}{path}", env.url))
        .bearer_auth(&env.token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
#[ignore]
async fn sessions_reach_the_server_s_history_stats_and_progress() {
    let env = env();
    let item = get_json(&env, &format!("/api/items/{}?expanded=1", env.item_id)).await;
    let duration = item["media"]["duration"].as_f64().unwrap_or(0.0);
    assert!(duration > 30.0, "pick a book longer than 30 s");

    let tmp = tempfile::tempdir().unwrap();
    let pool = abs_storage::connect_and_migrate(&tmp.path().join("db.sqlite3")).await.unwrap();
    let server_id = servers::add(&pool, &env.url).await.unwrap();
    let account_id = accounts::add(&pool, &server_id, "tester", &env.token, None).await.unwrap();
    let library_id = item["libraryId"].as_str().unwrap();
    libraries::upsert(
        &pool,
        libraries::UpsertLibrary { id: library_id, server_id: &server_id, name: "Books", media_type: "book", icon: None, display_order: 1 },
    )
    .await
    .unwrap();
    items::upsert(
        &pool,
        items::UpsertItem {
            id: &env.item_id,
            server_id: &server_id,
            library_id,
            title: item["media"]["metadata"]["title"].as_str().unwrap_or(""),
            author: None,
            narrator: None,
            description: None,
            duration_seconds: duration,
            added_at: chrono::Utc::now(),
            series_name: None,
            genres: &[],
        },
    )
    .await
    .unwrap();

    let api = abs_api::Client::with_bearer_token(&env.url, &env.token).unwrap();
    let device = device_info(&pool).await.unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let mut session = ListeningSession {
        id: abs_core::listening_sessions::new_session_id(),
        account_id: account_id.clone(),
        server_id: server_id.clone(),
        item_id: env.item_id.clone(),
        play_method: PLAY_METHOD_LOCAL,
        start_position: 5.0,
        current_position: 20.0,
        time_listening: 15.0,
        duration,
        started_at: now - 15_000,
        updated_at: now,
        sent_updated_at: None,
        closed: false,
    };

    // 1. A new session: stored, with this device, and its position applied.
    listening_sessions::save(&pool, &session).await.unwrap();
    let report = send_pending(&pool, &api, &device, &account_id, &server_id).await.unwrap();
    println!("first send: {report:?}");
    assert_eq!((report.sent, report.progress_updated), (1, 1));
    let history = get_json(&env, "/api/me/listening-sessions?itemsPerPage=50").await;
    let stored = history["sessions"].as_array().unwrap().iter().find(|s| s["id"] == session.id.as_str()).cloned().expect("in the history");
    println!("stored: {}", serde_json::to_string_pretty(&stored).unwrap());
    assert_eq!(stored["deviceInfo"]["clientName"], "Audiobooklet");
    assert_eq!(stored["deviceInfo"]["deviceId"], device.device_id.as_str());
    assert_eq!(stored["playMethod"], 3);
    assert_eq!(stored["timeListening"].as_f64(), Some(15.0));
    let progress = get_json(&env, &format!("/api/me/progress/{}", env.item_id)).await;
    assert_eq!(progress["currentTime"].as_f64(), Some(20.0));

    // 2. The same session, listened on: updated in place, not duplicated.
    session.current_position = 40.0;
    session.time_listening = 35.0;
    session.updated_at = chrono::Utc::now().timestamp_millis();
    listening_sessions::save(&pool, &session).await.unwrap();
    let report = send_pending(&pool, &api, &device, &account_id, &server_id).await.unwrap();
    assert_eq!((report.sent, report.progress_updated), (1, 1));
    let history = get_json(&env, "/api/me/listening-sessions?itemsPerPage=50").await;
    let copies: Vec<_> = history["sessions"].as_array().unwrap().iter().filter(|s| s["id"] == session.id.as_str()).collect();
    assert_eq!(copies.len(), 1, "updated, not duplicated");
    assert_eq!(copies[0]["timeListening"].as_f64(), Some(35.0));
    assert_eq!(get_json(&env, &format!("/api/me/progress/{}", env.item_id)).await["currentTime"].as_f64(), Some(40.0));

    // 3. A stale session (from an hour ago): stored in the history, but the newer progress stays.
    let stale_at = chrono::Utc::now().timestamp_millis() - 3_600_000;
    let stale = ListeningSession {
        id: abs_core::listening_sessions::new_session_id(),
        current_position: 10.0,
        start_position: 0.0,
        time_listening: 10.0,
        started_at: stale_at - 10_000,
        updated_at: stale_at,
        sent_updated_at: None,
        ..session.clone()
    };
    listening_sessions::save(&pool, &stale).await.unwrap();
    let report = send_pending(&pool, &api, &device, &account_id, &server_id).await.unwrap();
    println!("stale send: {report:?}");
    assert_eq!((report.sent, report.server_newer), (1, 1), "the server keeps its newer progress");
    assert_eq!(get_json(&env, &format!("/api/me/progress/{}", env.item_id)).await["currentTime"].as_f64(), Some(40.0));

    // 4. The listening counts in the stats.
    let stats = get_json(&env, "/api/me/listening-stats").await;
    println!("listening stats: totalTime={} today={}", stats["totalTime"], stats["today"]);
    assert!(stats["totalTime"].as_f64().unwrap_or(0.0) >= 35.0);

    let offset = abs_core::listening_sessions::clock_offsets().lock().unwrap().offset_ms(&server_id);
    println!("server clock offset: {offset:?} ms");
    assert!(offset.is_some(), "learned from the replies' Date header");
}
