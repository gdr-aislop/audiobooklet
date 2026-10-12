use chrono::Utc;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::error::{Result, StorageError};
use crate::models::Account;

/// Add a signed-in account for a server. Does not affect which account is active. `refresh_token`
/// is the JWT-auth refresh token (server v2.26.0+); `None` for legacy permanent-token servers.
pub async fn add(pool: &SqlitePool, server_id: &str, username: &str, token: &str, refresh_token: Option<&str>) -> Result<String> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();

    sqlx::query(
        "INSERT INTO accounts (id, server_id, username, token, refresh_token, is_active, created_at)
         VALUES (?, ?, ?, ?, ?, 0, ?)",
    )
    .bind(&id)
    .bind(server_id)
    .bind(username)
    .bind(token)
    .bind(refresh_token)
    .bind(now.to_rfc3339())
    .execute(pool)
    .await?;

    Ok(id)
}

/// Persist a freshly-rotated token pair: the access token the account should use from now on and
/// (rotated on every use, per the JWT auth system) its replacement refresh token. Both are
/// written together so a rotation can never leave the pair half-updated.
pub async fn set_tokens(pool: &SqlitePool, id: &str, token: &str, refresh_token: Option<&str>) -> Result<()> {
    let result = sqlx::query("UPDATE accounts SET token = ?, refresh_token = ? WHERE id = ?")
        .bind(token)
        .bind(refresh_token)
        .bind(id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(StorageError::NotFound(format!("account {id}")));
    }
    Ok(())
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Account> {
    sqlx::query_as(
        "SELECT id, server_id, username, token, refresh_token, is_active, created_at FROM accounts WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| StorageError::NotFound(format!("account {id}")))
}

pub async fn list_for_server(pool: &SqlitePool, server_id: &str) -> Result<Vec<Account>> {
    let accounts = sqlx::query_as(
        "SELECT id, server_id, username, token, refresh_token, is_active, created_at
         FROM accounts WHERE server_id = ? ORDER BY created_at ASC",
    )
    .bind(server_id)
    .fetch_all(pool)
    .await?;
    Ok(accounts)
}

/// The single globally-active account, if any (there is at most one across all servers — the
/// `accounts_one_active_idx` partial unique index in the schema enforces this at the DB level).
pub async fn get_active(pool: &SqlitePool) -> Result<Option<Account>> {
    let account = sqlx::query_as(
        "SELECT id, server_id, username, token, refresh_token, is_active, created_at
         FROM accounts WHERE is_active = 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(account)
}

/// Make `id` the active account, deactivating whichever account was previously active (which
/// may belong to a different server — this is how "switch server" is implemented). Runs in a
/// transaction so there is never a moment with zero or two active accounts visible to another
/// connection.
pub async fn set_active(pool: &SqlitePool, id: &str) -> Result<()> {
    let mut tx = pool.begin().await?;

    let exists: Option<String> = sqlx::query_scalar("SELECT id FROM accounts WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    if exists.is_none() {
        return Err(StorageError::NotFound(format!("account {id}")));
    }

    sqlx::query("UPDATE accounts SET is_active = 0 WHERE is_active = 1")
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE accounts SET is_active = 1, last_active_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(())
}

/// The account used most recently: the one last made active, or, among accounts never switched
/// to since that was recorded, the newest. What to fall back to when the active account goes.
pub async fn most_recent(pool: &SqlitePool) -> Result<Option<Account>> {
    let account = sqlx::query_as(
        "SELECT id, server_id, username, token, refresh_token, is_active, created_at
         FROM accounts ORDER BY last_active_at IS NULL, last_active_at DESC, created_at DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(account)
}

/// Sign out: remove the account entirely (its progress rows cascade-delete with it — progress
/// is per-account, so this is the correct behavior, not a data leak).
pub async fn remove(pool: &SqlitePool, id: &str) -> Result<()> {
    let result = sqlx::query("DELETE FROM accounts WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(StorageError::NotFound(format!("account {id}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connect_and_migrate;
    use crate::repo::servers;

    async fn pool_with_server() -> (SqlitePool, String) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = connect_and_migrate(&tmp.path().join("db.sqlite3"))
            .await
            .unwrap();
        std::mem::forget(tmp);
        let server_id = servers::add(&pool, "https://a.example").await.unwrap();
        (pool, server_id)
    }

    #[tokio::test]
    async fn add_then_get_round_trips() {
        let (pool, server_id) = pool_with_server().await;
        let id = add(&pool, &server_id, "jane", "tok123", Some("refresh789")).await.unwrap();

        let account = get(&pool, &id).await.unwrap();
        assert_eq!(account.username, "jane");
        assert_eq!(account.token, "tok123");
        assert_eq!(account.refresh_token.as_deref(), Some("refresh789"));
        assert!(!account.is_active, "new accounts start inactive");
    }

    #[tokio::test]
    async fn legacy_accounts_have_no_refresh_token() {
        let (pool, server_id) = pool_with_server().await;
        let id = add(&pool, &server_id, "jane", "tok123", None).await.unwrap();
        assert!(get(&pool, &id).await.unwrap().refresh_token.is_none());
    }

    #[tokio::test]
    async fn set_tokens_rotates_both_tokens_together() {
        let (pool, server_id) = pool_with_server().await;
        let id = add(&pool, &server_id, "jane", "tok1", Some("refresh1")).await.unwrap();

        set_tokens(&pool, &id, "tok2", Some("refresh2")).await.unwrap();
        let account = get(&pool, &id).await.unwrap();
        assert_eq!(account.token, "tok2");
        assert_eq!(account.refresh_token.as_deref(), Some("refresh2"));

        set_tokens(&pool, &id, "tok3", None).await.unwrap();
        let account = get(&pool, &id).await.unwrap();
        assert_eq!(account.token, "tok3");
        assert!(account.refresh_token.is_none(), "a server that stops sending refresh tokens is honored");

        let err = set_tokens(&pool, "no-such-account", "tok", None).await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn adding_account_for_missing_server_fails_fk_check() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = connect_and_migrate(&tmp.path().join("db.sqlite3"))
            .await
            .unwrap();
        let result = add(&pool, "no-such-server", "jane", "tok", None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn set_active_activates_exactly_one_account() {
        let (pool, server_id) = pool_with_server().await;
        let a = add(&pool, &server_id, "a", "tok-a", None).await.unwrap();
        let b = add(&pool, &server_id, "b", "tok-b", None).await.unwrap();

        set_active(&pool, &a).await.unwrap();
        assert!(get(&pool, &a).await.unwrap().is_active);
        assert!(!get(&pool, &b).await.unwrap().is_active);
        assert_eq!(get_active(&pool).await.unwrap().unwrap().id, a);

        set_active(&pool, &b).await.unwrap();
        assert!(!get(&pool, &a).await.unwrap().is_active);
        assert!(get(&pool, &b).await.unwrap().is_active);
        assert_eq!(get_active(&pool).await.unwrap().unwrap().id, b);
    }

    #[tokio::test]
    async fn set_active_switches_across_servers() {
        let (pool, server_a) = pool_with_server().await;
        let server_b = servers::add(&pool, "https://b.example").await.unwrap();

        let acct_a = add(&pool, &server_a, "a", "tok-a", None).await.unwrap();
        let acct_b = add(&pool, &server_b, "b", "tok-b", None).await.unwrap();

        set_active(&pool, &acct_a).await.unwrap();
        set_active(&pool, &acct_b).await.unwrap();

        assert!(!get(&pool, &acct_a).await.unwrap().is_active);
        assert!(get(&pool, &acct_b).await.unwrap().is_active);
    }

    #[tokio::test]
    async fn set_active_on_missing_account_fails_and_changes_nothing() {
        let (pool, server_id) = pool_with_server().await;
        let a = add(&pool, &server_id, "a", "tok-a", None).await.unwrap();
        set_active(&pool, &a).await.unwrap();

        let err = set_active(&pool, "does-not-exist").await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
        // The previously-active account must still be active — a failed switch shouldn't
        // silently deactivate everything.
        assert!(get(&pool, &a).await.unwrap().is_active);
    }

    #[tokio::test]
    async fn most_recent_is_the_account_last_made_active() {
        let (pool, server_id) = pool_with_server().await;
        assert!(most_recent(&pool).await.unwrap().is_none());
        let a = add(&pool, &server_id, "a", "tok-a", None).await.unwrap();
        let b = add(&pool, &server_id, "b", "tok-b", None).await.unwrap();
        assert_eq!(most_recent(&pool).await.unwrap().unwrap().id, b, "never activated: the newest");

        set_active(&pool, &b).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        set_active(&pool, &a).await.unwrap();
        assert_eq!(most_recent(&pool).await.unwrap().unwrap().id, a);
        remove(&pool, &a).await.unwrap();
        assert_eq!(most_recent(&pool).await.unwrap().unwrap().id, b, "then the one used before it");
    }

    #[tokio::test]
    async fn get_active_is_none_when_no_account_is_active() {
        let (pool, server_id) = pool_with_server().await;
        add(&pool, &server_id, "a", "tok-a", None).await.unwrap();
        assert!(get_active(&pool).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn remove_deletes_the_account() {
        let (pool, server_id) = pool_with_server().await;
        let id = add(&pool, &server_id, "a", "tok-a", None).await.unwrap();
        remove(&pool, &id).await.unwrap();
        assert!(get(&pool, &id).await.is_err());
    }

    #[tokio::test]
    async fn removing_a_server_cascades_to_its_accounts() {
        let (pool, server_id) = pool_with_server().await;
        let id = add(&pool, &server_id, "a", "tok-a", None).await.unwrap();

        servers::remove(&pool, &server_id).await.unwrap();

        assert!(get(&pool, &id).await.is_err(), "account should be gone too");
    }

    #[tokio::test]
    async fn list_for_server_only_returns_that_servers_accounts() {
        let (pool, server_a) = pool_with_server().await;
        let server_b = servers::add(&pool, "https://b.example").await.unwrap();

        add(&pool, &server_a, "a1", "t", None).await.unwrap();
        add(&pool, &server_a, "a2", "t", None).await.unwrap();
        add(&pool, &server_b, "b1", "t", None).await.unwrap();

        let a_accounts = list_for_server(&pool, &server_a).await.unwrap();
        let b_accounts = list_for_server(&pool, &server_b).await.unwrap();

        assert_eq!(a_accounts.len(), 2);
        assert_eq!(b_accounts.len(), 1);
    }

    #[tokio::test]
    async fn duplicate_username_on_same_server_is_rejected() {
        let (pool, server_id) = pool_with_server().await;
        add(&pool, &server_id, "jane", "tok1", None).await.unwrap();
        let result = add(&pool, &server_id, "jane", "tok2", None).await;
        assert!(result.is_err(), "UNIQUE(server_id, username) should reject this");
    }
}
