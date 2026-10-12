use chrono::Utc;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::error::{Result, StorageError};
use crate::models::Server;

/// Register a new server connection. Returns the generated id.
pub async fn add(pool: &SqlitePool, url: &str) -> Result<String> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();

    sqlx::query(
        "INSERT INTO servers (id, url, custom_headers_json, disable_ssl_verify, created_at)
         VALUES (?, ?, '{}', 0, ?)",
    )
    .bind(&id)
    .bind(url)
    .bind(now.to_rfc3339())
    .execute(pool)
    .await?;

    Ok(id)
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Server> {
    sqlx::query_as(
        "SELECT id, url, custom_headers_json, disable_ssl_verify, client_cert_path,
                client_cert_password, local_network_address, user_agent, created_at
         FROM servers WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| StorageError::NotFound(format!("server {id}")))
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<Server>> {
    let servers = sqlx::query_as(
        "SELECT id, url, custom_headers_json, disable_ssl_verify, client_cert_path,
                client_cert_password, local_network_address, user_agent, created_at
         FROM servers ORDER BY created_at ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(servers)
}

/// Servers whose address is `url` once both are normalized the way logins store them (trimmed,
/// no trailing slash), oldest first.
pub async fn find_by_url(pool: &SqlitePool, url: &str) -> Result<Vec<Server>> {
    let wanted = normalize_url(url);
    Ok(list(pool).await?.into_iter().filter(|server| normalize_url(&server.url) == wanted).collect())
}

/// A server address as typed, in the form it's compared and stored in: surrounding spaces and
/// trailing slashes don't make a different server.
pub fn normalize_url(url: &str) -> &str {
    url.trim().trim_end_matches('/')
}

pub async fn remove(pool: &SqlitePool, id: &str) -> Result<()> {
    // ON DELETE CASCADE takes care of accounts/libraries/items/etc. for this server.
    let result = sqlx::query("DELETE FROM servers WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(StorageError::NotFound(format!("server {id}")));
    }
    Ok(())
}

pub async fn set_disable_ssl_verify(pool: &SqlitePool, id: &str, disable: bool) -> Result<()> {
    let result = sqlx::query("UPDATE servers SET disable_ssl_verify = ? WHERE id = ?")
        .bind(disable)
        .bind(id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(StorageError::NotFound(format!("server {id}")));
    }
    Ok(())
}

pub async fn set_local_network_address(
    pool: &SqlitePool,
    id: &str,
    address: Option<&str>,
) -> Result<()> {
    let result = sqlx::query("UPDATE servers SET local_network_address = ? WHERE id = ?")
        .bind(address)
        .bind(id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(StorageError::NotFound(format!("server {id}")));
    }
    Ok(())
}

pub async fn set_custom_headers_json(pool: &SqlitePool, id: &str, json: &str) -> Result<()> {
    let result = sqlx::query("UPDATE servers SET custom_headers_json = ? WHERE id = ?")
        .bind(json)
        .bind(id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(StorageError::NotFound(format!("server {id}")));
    }
    Ok(())
}

pub async fn set_client_cert_path(pool: &SqlitePool, id: &str, path: Option<&str>) -> Result<()> {
    let result = sqlx::query("UPDATE servers SET client_cert_path = ? WHERE id = ?")
        .bind(path)
        .bind(id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(StorageError::NotFound(format!("server {id}")));
    }
    Ok(())
}

pub async fn set_client_cert_password(
    pool: &SqlitePool,
    id: &str,
    password: Option<&str>,
) -> Result<()> {
    let result = sqlx::query("UPDATE servers SET client_cert_password = ? WHERE id = ?")
        .bind(password)
        .bind(id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(StorageError::NotFound(format!("server {id}")));
    }
    Ok(())
}

pub async fn set_user_agent(pool: &SqlitePool, id: &str, user_agent: Option<&str>) -> Result<()> {
    let result = sqlx::query("UPDATE servers SET user_agent = ? WHERE id = ?")
        .bind(user_agent)
        .bind(id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(StorageError::NotFound(format!("server {id}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connect_and_migrate;

    async fn pool() -> SqlitePool {
        let tmp = tempfile::tempdir().unwrap();
        // Leak the tempdir for the pool's lifetime within the test; each test gets its own.
        let path = tmp.path().join("db.sqlite3");
        let pool = connect_and_migrate(&path).await.unwrap();
        std::mem::forget(tmp);
        pool
    }

    #[tokio::test]
    async fn find_by_url_ignores_spaces_and_trailing_slashes() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = crate::db::connect_and_migrate(&tmp.path().join("db.sqlite3")).await.unwrap();
        let a = add(&pool, "http://host:13378").await.unwrap();
        add(&pool, "http://host:13379/").await.unwrap();
        let found: Vec<String> = find_by_url(&pool, " http://host:13378/ ").await.unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(found, [a]);
        assert!(find_by_url(&pool, "https://host:13378").await.unwrap().is_empty(), "another scheme is another address");
    }

    #[tokio::test]
    async fn add_then_get_round_trips() {
        let pool = pool().await;
        let id = add(&pool, "https://library.homeserver.dev").await.unwrap();

        let server = get(&pool, &id).await.unwrap();
        assert_eq!(server.id, id);
        assert_eq!(server.url, "https://library.homeserver.dev");
        assert!(!server.disable_ssl_verify);
        assert_eq!(server.client_cert_path, None);
    }

    #[tokio::test]
    async fn get_missing_server_returns_not_found() {
        let pool = pool().await;
        let err = get(&pool, "does-not-exist").await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn list_returns_servers_in_creation_order() {
        let pool = pool().await;
        let first = add(&pool, "https://a.example").await.unwrap();
        let second = add(&pool, "https://b.example").await.unwrap();

        let servers = list(&pool).await.unwrap();
        let ids: Vec<_> = servers.iter().map(|s| s.id.clone()).collect();
        assert_eq!(ids, vec![first, second]);
    }

    #[tokio::test]
    async fn remove_deletes_the_server() {
        let pool = pool().await;
        let id = add(&pool, "https://a.example").await.unwrap();
        remove(&pool, &id).await.unwrap();
        assert!(get(&pool, &id).await.is_err());
    }

    #[tokio::test]
    async fn remove_missing_server_is_an_error() {
        let pool = pool().await;
        let err = remove(&pool, "does-not-exist").await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn set_disable_ssl_verify_updates_the_flag() {
        let pool = pool().await;
        let id = add(&pool, "https://a.example").await.unwrap();

        set_disable_ssl_verify(&pool, &id, true).await.unwrap();
        assert!(get(&pool, &id).await.unwrap().disable_ssl_verify);

        set_disable_ssl_verify(&pool, &id, false).await.unwrap();
        assert!(!get(&pool, &id).await.unwrap().disable_ssl_verify);
    }

    #[tokio::test]
    async fn set_local_network_address_can_be_cleared() {
        let pool = pool().await;
        let id = add(&pool, "https://a.example").await.unwrap();

        set_local_network_address(&pool, &id, Some("192.168.1.50"))
            .await
            .unwrap();
        assert_eq!(
            get(&pool, &id).await.unwrap().local_network_address,
            Some("192.168.1.50".to_string())
        );

        set_local_network_address(&pool, &id, None).await.unwrap();
        assert_eq!(get(&pool, &id).await.unwrap().local_network_address, None);
    }

    #[tokio::test]
    async fn set_custom_headers_json_round_trips() {
        let pool = pool().await;
        let id = add(&pool, "https://a.example").await.unwrap();
        // `add` seeds an empty JSON object — a server starts with no custom headers, not NULL.
        assert_eq!(get(&pool, &id).await.unwrap().custom_headers_json, "{}");

        set_custom_headers_json(&pool, &id, r#"{"X-Auth":"secret"}"#).await.unwrap();
        assert_eq!(get(&pool, &id).await.unwrap().custom_headers_json, r#"{"X-Auth":"secret"}"#);

        set_custom_headers_json(&pool, &id, "{}").await.unwrap();
        assert_eq!(get(&pool, &id).await.unwrap().custom_headers_json, "{}");
    }

    #[tokio::test]
    async fn set_client_cert_path_and_password_round_trip_and_clear() {
        let pool = pool().await;
        let id = add(&pool, "https://a.example").await.unwrap();

        set_client_cert_path(&pool, &id, Some("/certs/client.p12")).await.unwrap();
        set_client_cert_password(&pool, &id, Some("export-password")).await.unwrap();
        let server = get(&pool, &id).await.unwrap();
        assert_eq!(server.client_cert_path.as_deref(), Some("/certs/client.p12"));
        assert_eq!(server.client_cert_password.as_deref(), Some("export-password"));

        // Clearing the path clears the password too in normal use (the UI clears both
        // together), but the setters are independent — clearing the password alone is legal.
        set_client_cert_password(&pool, &id, None).await.unwrap();
        let server = get(&pool, &id).await.unwrap();
        assert_eq!(server.client_cert_path.as_deref(), Some("/certs/client.p12"));
        assert_eq!(server.client_cert_password, None);

        set_client_cert_path(&pool, &id, None).await.unwrap();
        assert_eq!(get(&pool, &id).await.unwrap().client_cert_path, None);
    }

    #[tokio::test]
    async fn set_user_agent_round_trips() {
        let pool = pool().await;
        let id = add(&pool, "https://a.example").await.unwrap();
        assert_eq!(get(&pool, &id).await.unwrap().user_agent, None);

        set_user_agent(&pool, &id, Some("MyAgent/1.0")).await.unwrap();
        assert_eq!(get(&pool, &id).await.unwrap().user_agent.as_deref(), Some("MyAgent/1.0"));

        set_user_agent(&pool, &id, None).await.unwrap();
        assert_eq!(get(&pool, &id).await.unwrap().user_agent, None);
    }

    #[tokio::test]
    async fn setters_on_a_missing_server_are_an_error() {
        let pool = pool().await;
        for error in [
            set_custom_headers_json(&pool, "nope", "{}").await.unwrap_err(),
            set_client_cert_path(&pool, "nope", None).await.unwrap_err(),
            set_client_cert_password(&pool, "nope", None).await.unwrap_err(),
            set_user_agent(&pool, "nope", None).await.unwrap_err(),
        ] {
            assert!(matches!(error, StorageError::NotFound(_)));
        }
    }
}
