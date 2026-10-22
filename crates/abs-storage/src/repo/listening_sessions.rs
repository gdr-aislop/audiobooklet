//! Listening sessions recorded on this device (migration 0012). Recording them is the player's
//! job and sending them to the server is `abs-core::listening_sessions`'s; this module only
//! persists them.

use sqlx::SqlitePool;

use crate::error::Result;
use crate::models::ListeningSession;

const COLUMNS: &str = "id, account_id, server_id, item_id, play_method, start_position, current_position, time_listening, duration, \
                       started_at, updated_at, sent_updated_at, closed";

/// Inserts or updates a session as the player has it now. Never touches `sent_updated_at`: what
/// the server has confirmed is [`mark_sent`]'s to record, so a save landing while a send is in
/// flight leaves the row dirty.
pub async fn save(pool: &SqlitePool, session: &ListeningSession) -> Result<()> {
    sqlx::query(
        "INSERT INTO listening_sessions
            (id, account_id, server_id, item_id, play_method, start_position, current_position, time_listening, duration, started_at, updated_at, closed)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
            play_method = excluded.play_method,
            current_position = excluded.current_position,
            time_listening = excluded.time_listening,
            duration = excluded.duration,
            updated_at = excluded.updated_at,
            closed = excluded.closed",
    )
    .bind(&session.id)
    .bind(&session.account_id)
    .bind(&session.server_id)
    .bind(&session.item_id)
    .bind(session.play_method)
    .bind(session.start_position)
    .bind(session.current_position)
    .bind(session.time_listening)
    .bind(session.duration)
    .bind(session.started_at)
    .bind(session.updated_at)
    .bind(session.closed)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<ListeningSession>> {
    let session = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {COLUMNS} FROM listening_sessions WHERE id = ?")))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(session)
}

/// Sessions of one account on one server with something the server hasn't confirmed, oldest
/// change first, at most `limit` of them.
pub async fn list_dirty(pool: &SqlitePool, account_id: &str, server_id: &str, limit: i64) -> Result<Vec<ListeningSession>> {
    let sessions = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM listening_sessions
         WHERE account_id = ? AND server_id = ? AND sent_updated_at IS NOT updated_at
         ORDER BY updated_at LIMIT ?"
    )))
    .bind(account_id)
    .bind(server_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(sessions)
}

/// How many sessions of one account on one server are waiting to be sent.
pub async fn count_dirty(pool: &SqlitePool, account_id: &str, server_id: &str) -> Result<i64> {
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM listening_sessions WHERE account_id = ? AND server_id = ? AND sent_updated_at IS NOT updated_at",
    )
    .bind(account_id)
    .bind(server_id)
    .fetch_one(pool)
    .await?;
    Ok(count.0)
}

/// Records that the server has the session as of `updated_at` (the value that was sent). A save
/// that landed meanwhile has a later `updated_at`, so the row stays dirty and is sent again.
pub async fn mark_sent(pool: &SqlitePool, id: &str, updated_at: i64) -> Result<()> {
    sqlx::query("UPDATE listening_sessions SET sent_updated_at = ? WHERE id = ?")
        .bind(updated_at)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Closes every session still open — at startup, for sessions a crash or a kill left open: the
/// listening they hold is kept, but a new play starts a new session.
pub async fn close_all_open(pool: &SqlitePool) -> Result<u64> {
    let result = sqlx::query("UPDATE listening_sessions SET closed = 1 WHERE closed = 0").execute(pool).await?;
    Ok(result.rows_affected())
}

pub async fn delete(pool: &SqlitePool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM listening_sessions WHERE id = ?").bind(id).execute(pool).await?;
    Ok(())
}

/// Deletes closed sessions the server has in full and that ended before `before_ms`: they're
/// only kept a while so a "did it reach the server" question can still be answered from the
/// database.
pub async fn prune_sent(pool: &SqlitePool, before_ms: i64) -> Result<u64> {
    let result = sqlx::query(
        "DELETE FROM listening_sessions WHERE closed = 1 AND sent_updated_at = updated_at AND updated_at < ?",
    )
    .bind(before_ms)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connect_and_migrate;
    use crate::repo::{accounts, items, libraries, servers};
    use chrono::Utc;

    async fn pool_with_item_and_account() -> (SqlitePool, String, String) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = connect_and_migrate(&tmp.path().join("db.sqlite3")).await.unwrap();
        std::mem::forget(tmp);
        let server_id = servers::add(&pool, "https://a.example").await.unwrap();
        let account_id = accounts::add(&pool, &server_id, "jane", "tok", None).await.unwrap();
        libraries::upsert(
            &pool,
            libraries::UpsertLibrary { id: "lib-1", server_id: &server_id, name: "Audiobooks", media_type: "book", icon: None, display_order: 1 },
        )
        .await
        .unwrap();
        for id in ["item-1", "item-2"] {
            items::upsert(
                &pool,
                items::UpsertItem {
                    id,
                    server_id: &server_id,
                    library_id: "lib-1",
                    title: "Book",
                    author: None,
                    narrator: None,
                    description: None,
                    duration_seconds: 3600.0,
                    added_at: Utc::now(),
                    series_name: None,
                    genres: &[],
                },
            )
            .await
            .unwrap();
        }
        (pool, server_id, account_id)
    }

    fn session(id: &str, account_id: &str, server_id: &str, item_id: &str, updated_at: i64) -> ListeningSession {
        ListeningSession {
            id: id.to_string(),
            account_id: account_id.to_string(),
            server_id: server_id.to_string(),
            item_id: item_id.to_string(),
            play_method: 3,
            start_position: 100.0,
            current_position: 160.0,
            time_listening: 60.0,
            duration: 3600.0,
            started_at: updated_at - 60_000,
            updated_at,
            sent_updated_at: None,
            closed: false,
        }
    }

    #[tokio::test]
    async fn a_saved_session_is_dirty_until_sent_and_again_after_a_newer_save() {
        let (pool, server_id, account_id) = pool_with_item_and_account().await;
        let mut s = session("s-1", &account_id, &server_id, "item-1", 1_000_000);
        save(&pool, &s).await.unwrap();
        assert_eq!(get(&pool, "s-1").await.unwrap().unwrap(), s);
        assert_eq!(list_dirty(&pool, &account_id, &server_id, 50).await.unwrap().len(), 1);

        mark_sent(&pool, "s-1", 1_000_000).await.unwrap();
        assert!(list_dirty(&pool, &account_id, &server_id, 50).await.unwrap().is_empty());
        assert_eq!(count_dirty(&pool, &account_id, &server_id).await.unwrap(), 0);

        // Listening went on: dirty again, and the save keeps what the server confirmed.
        s.current_position = 220.0;
        s.time_listening = 120.0;
        s.updated_at = 1_060_000;
        save(&pool, &s).await.unwrap();
        let stored = get(&pool, "s-1").await.unwrap().unwrap();
        assert_eq!(stored.sent_updated_at, Some(1_000_000));
        assert_eq!(stored.current_position, 220.0);
        assert_eq!(count_dirty(&pool, &account_id, &server_id).await.unwrap(), 1);

        // A send of the older state that completes late doesn't clear the newer one.
        mark_sent(&pool, "s-1", 1_000_000).await.unwrap();
        assert_eq!(count_dirty(&pool, &account_id, &server_id).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn list_dirty_is_per_account_and_server_oldest_first_and_limited() {
        let (pool, server_id, account_id) = pool_with_item_and_account().await;
        let other_account = accounts::add(&pool, &server_id, "john", "tok", None).await.unwrap();
        save(&pool, &session("late", &account_id, &server_id, "item-1", 3_000)).await.unwrap();
        save(&pool, &session("early", &account_id, &server_id, "item-2", 1_000)).await.unwrap();
        save(&pool, &session("theirs", &other_account, &server_id, "item-1", 2_000)).await.unwrap();

        let ids: Vec<String> = list_dirty(&pool, &account_id, &server_id, 50).await.unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["early", "late"]);
        assert_eq!(list_dirty(&pool, &account_id, &server_id, 1).await.unwrap().len(), 1);
        assert!(list_dirty(&pool, &account_id, "other-server", 50).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn close_all_open_and_prune_only_drop_what_the_server_has() {
        let (pool, server_id, account_id) = pool_with_item_and_account().await;
        save(&pool, &session("sent-old", &account_id, &server_id, "item-1", 1_000)).await.unwrap();
        save(&pool, &session("unsent-old", &account_id, &server_id, "item-1", 1_000)).await.unwrap();
        save(&pool, &session("sent-new", &account_id, &server_id, "item-2", 9_000)).await.unwrap();
        mark_sent(&pool, "sent-old", 1_000).await.unwrap();
        mark_sent(&pool, "sent-new", 9_000).await.unwrap();

        assert_eq!(prune_sent(&pool, 5_000).await.unwrap(), 0, "open sessions are never pruned");
        assert_eq!(close_all_open(&pool).await.unwrap(), 3);
        assert_eq!(prune_sent(&pool, 5_000).await.unwrap(), 1);
        assert!(get(&pool, "sent-old").await.unwrap().is_none());
        assert!(get(&pool, "unsent-old").await.unwrap().is_some(), "not sent yet: kept");
        assert!(get(&pool, "sent-new").await.unwrap().is_some(), "too recent: kept");
    }

    #[tokio::test]
    async fn sessions_go_with_their_book_and_their_account() {
        let (pool, server_id, account_id) = pool_with_item_and_account().await;
        save(&pool, &session("s-1", &account_id, &server_id, "item-1", 1_000)).await.unwrap();
        save(&pool, &session("s-2", &account_id, &server_id, "item-2", 1_000)).await.unwrap();

        sqlx::query("DELETE FROM items WHERE id = 'item-1'").execute(&pool).await.unwrap();
        assert!(get(&pool, "s-1").await.unwrap().is_none());
        accounts::remove(&pool, &account_id).await.unwrap();
        assert!(get(&pool, "s-2").await.unwrap().is_none());
    }
}
