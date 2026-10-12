//! Libraries are cached from the server, never created locally, so the only write operation is
//! an idempotent upsert (as run by `abs-core`'s sync) rather than a create/update pair.

use chrono::Utc;
use sqlx::SqlitePool;

use crate::error::{Result, StorageError};
use crate::models::Library;

pub struct UpsertLibrary<'a> {
    pub id: &'a str,
    pub server_id: &'a str,
    pub name: &'a str,
    pub media_type: &'a str,
    pub icon: Option<&'a str>,
    pub display_order: i64,
}

pub async fn upsert(pool: &SqlitePool, lib: UpsertLibrary<'_>) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO libraries (id, server_id, name, media_type, icon, display_order, synced_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(server_id, id) DO UPDATE SET
            name = excluded.name,
            media_type = excluded.media_type,
            icon = excluded.icon,
            display_order = excluded.display_order,
            synced_at = excluded.synced_at,
            removed_on_server = 0",
    )
    .bind(lib.id)
    .bind(lib.server_id)
    .bind(lib.name)
    .bind(lib.media_type)
    .bind(lib.icon)
    .bind(lib.display_order)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get(pool: &SqlitePool, server_id: &str, id: &str) -> Result<Library> {
    sqlx::query_as(
        "SELECT id, server_id, name, media_type, icon, display_order, synced_at, removed_on_server
         FROM libraries WHERE server_id = ? AND id = ?",
    )
    .bind(server_id)
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| StorageError::NotFound(format!("library {id} on server {server_id}")))
}

pub async fn list_for_server(pool: &SqlitePool, server_id: &str) -> Result<Vec<Library>> {
    let libraries = sqlx::query_as(
        "SELECT id, server_id, name, media_type, icon, display_order, synced_at, removed_on_server
         FROM libraries WHERE server_id = ? ORDER BY display_order ASC",
    )
    .bind(server_id)
    .fetch_all(pool)
    .await?;
    Ok(libraries)
}

/// Marks a library the server no longer lists but that still holds downloaded books (see the
/// `removed_on_server` column).
pub async fn set_removed_on_server(pool: &SqlitePool, server_id: &str, id: &str) -> Result<()> {
    sqlx::query("UPDATE libraries SET removed_on_server = 1 WHERE server_id = ? AND id = ?")
        .bind(server_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn remove(pool: &SqlitePool, server_id: &str, id: &str) -> Result<()> {
    let result = sqlx::query("DELETE FROM libraries WHERE server_id = ? AND id = ?")
        .bind(server_id)
        .bind(id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(StorageError::NotFound(format!(
            "library {id} on server {server_id}"
        )));
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

    fn lib<'a>(server_id: &'a str, id: &'a str, name: &'a str) -> UpsertLibrary<'a> {
        UpsertLibrary {
            id,
            server_id,
            name,
            media_type: "book",
            icon: Some("audiobookshelf"),
            display_order: 1,
        }
    }

    #[tokio::test]
    async fn upsert_then_get_round_trips() {
        let (pool, server_id) = pool_with_server().await;
        upsert(&pool, lib(&server_id, "lib-1", "Audiobooks")).await.unwrap();

        let library = get(&pool, &server_id, "lib-1").await.unwrap();
        assert_eq!(library.name, "Audiobooks");
        assert_eq!(library.media_type, "book");
    }

    #[tokio::test]
    async fn upsert_twice_updates_rather_than_duplicating() {
        let (pool, server_id) = pool_with_server().await;
        upsert(&pool, lib(&server_id, "lib-1", "Audiobooks")).await.unwrap();
        upsert(&pool, lib(&server_id, "lib-1", "Renamed Library")).await.unwrap();

        let libraries = list_for_server(&pool, &server_id).await.unwrap();
        assert_eq!(libraries.len(), 1, "should still be exactly one row");
        assert_eq!(libraries[0].name, "Renamed Library");
    }

    #[tokio::test]
    async fn same_library_id_on_different_servers_does_not_collide() {
        let (pool, server_a) = pool_with_server().await;
        let server_b = servers::add(&pool, "https://b.example").await.unwrap();

        upsert(&pool, lib(&server_a, "same-id", "A's Library")).await.unwrap();
        upsert(&pool, lib(&server_b, "same-id", "B's Library")).await.unwrap();

        assert_eq!(get(&pool, &server_a, "same-id").await.unwrap().name, "A's Library");
        assert_eq!(get(&pool, &server_b, "same-id").await.unwrap().name, "B's Library");
    }

    #[tokio::test]
    async fn list_for_server_orders_by_display_order() {
        let (pool, server_id) = pool_with_server().await;
        upsert(
            &pool,
            UpsertLibrary {
                display_order: 2,
                ..lib(&server_id, "second", "Second")
            },
        )
        .await
        .unwrap();
        upsert(
            &pool,
            UpsertLibrary {
                display_order: 1,
                ..lib(&server_id, "first", "First")
            },
        )
        .await
        .unwrap();

        let libraries = list_for_server(&pool, &server_id).await.unwrap();
        let names: Vec<_> = libraries.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, vec!["First", "Second"]);
    }

    #[tokio::test]
    async fn get_missing_library_is_not_found() {
        let (pool, server_id) = pool_with_server().await;
        let err = get(&pool, &server_id, "no-such-lib").await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn remove_deletes_the_library() {
        let (pool, server_id) = pool_with_server().await;
        upsert(&pool, lib(&server_id, "lib-1", "Audiobooks")).await.unwrap();
        remove(&pool, &server_id, "lib-1").await.unwrap();
        assert!(get(&pool, &server_id, "lib-1").await.is_err());
    }

    #[tokio::test]
    async fn removing_a_server_cascades_to_its_libraries() {
        let (pool, server_id) = pool_with_server().await;
        upsert(&pool, lib(&server_id, "lib-1", "Audiobooks")).await.unwrap();

        servers::remove(&pool, &server_id).await.unwrap();

        assert!(get(&pool, &server_id, "lib-1").await.is_err());
    }
}
