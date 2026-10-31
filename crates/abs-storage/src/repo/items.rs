//! Items are cached from the server (like libraries), so writes are idempotent upserts.

use chrono::Utc;
use sqlx::SqlitePool;

use crate::error::{Result, StorageError};
use crate::models::{Item, ItemExtras};

pub struct UpsertItem<'a> {
    pub id: &'a str,
    pub server_id: &'a str,
    pub library_id: &'a str,
    pub title: &'a str,
    pub author: Option<&'a str>,
    pub narrator: Option<&'a str>,
    pub description: Option<&'a str>,
    pub duration_seconds: f64,
    pub added_at: chrono::DateTime<Utc>,
    pub series_name: Option<&'a str>,
    pub genres: &'a [String],
}

pub async fn upsert<'e>(db: impl sqlx::Executor<'e, Database = sqlx::Sqlite>, item: UpsertItem<'_>) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let genres_json = serde_json::to_string(item.genres).unwrap_or_else(|_| "[]".to_string());
    sqlx::query(
        "INSERT INTO items
            (id, server_id, library_id, title, author, narrator, description,
             duration_seconds, added_at, synced_at, series_name, genres)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(server_id, id) DO UPDATE SET
            library_id = excluded.library_id,
            title = excluded.title,
            author = excluded.author,
            narrator = excluded.narrator,
            description = excluded.description,
            duration_seconds = excluded.duration_seconds,
            added_at = excluded.added_at,
            synced_at = excluded.synced_at,
            series_name = excluded.series_name,
            genres = excluded.genres",
    )
    .bind(item.id)
    .bind(item.server_id)
    .bind(item.library_id)
    .bind(item.title)
    .bind(item.author)
    .bind(item.narrator)
    .bind(item.description)
    .bind(item.duration_seconds)
    .bind(item.added_at.to_rfc3339())
    .bind(now)
    .bind(item.series_name)
    .bind(genres_json)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn get(pool: &SqlitePool, server_id: &str, id: &str) -> Result<Item> {
    sqlx::query_as(
        "SELECT id, server_id, library_id, title, author, narrator, description,
                cover_cache_path, duration_seconds, added_at, synced_at,
                series_name, genres AS genres_json, title_sort, author_sort
         FROM items WHERE server_id = ? AND id = ?",
    )
    .bind(server_id)
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| StorageError::NotFound(format!("item {id} on server {server_id}")))
}

/// Saves what the library list returned beyond the basic fields (see [`ItemExtras`]) on an item
/// [`upsert`] has already written. Like `upsert`, takes a pool or a transaction. A separate write so the many callers that only know the basics
/// stay as they are; replaces every extra (the server is the authority, and a field it stopped
/// sending is cleared). Changes nothing if the item doesn't exist.
pub async fn set_extras<'e>(db: impl sqlx::Executor<'e, Database = sqlx::Sqlite>, server_id: &str, id: &str, extras: &ItemExtras) -> Result<()> {
    sqlx::query(
        "UPDATE items SET subtitle = ?, size_bytes = ?, num_chapters = ?, tags = ?, published_year = ?,
            publisher = ?, language = ?, explicit = ?, isbn = ?, asin = ?, has_cover = ?, title_sort = ?, author_sort = ?
         WHERE server_id = ? AND id = ?",
    )
    .bind(&extras.subtitle)
    .bind(extras.size_bytes)
    .bind(extras.num_chapters)
    .bind(&extras.tags_json)
    .bind(&extras.published_year)
    .bind(&extras.publisher)
    .bind(&extras.language)
    .bind(extras.explicit)
    .bind(&extras.isbn)
    .bind(&extras.asin)
    .bind(extras.has_cover)
    .bind(&extras.title_sort)
    .bind(&extras.author_sort)
    .bind(server_id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn get_extras(pool: &SqlitePool, server_id: &str, id: &str) -> Result<ItemExtras> {
    sqlx::query_as(
        "SELECT subtitle, size_bytes, num_chapters, tags AS tags_json, published_year, publisher, language,
                explicit, isbn, asin, has_cover, title_sort, author_sort
         FROM items WHERE server_id = ? AND id = ?",
    )
    .bind(server_id)
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| StorageError::NotFound(format!("item {id} on server {server_id}")))
}

/// What the cover fetch needs to know about one item: the cached file's path, and whether the
/// server's list said it has a cover at all.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct CoverState {
    pub id: String,
    pub cover_cache_path: Option<String>,
    pub has_cover: Option<bool>,
}

/// [`CoverState`] of every item of a server, in one query. A cover batch used to read each item's
/// whole row (descriptions included) one at a time just to learn this, 249 reads for 249 books.
pub async fn cover_states_for_server(pool: &SqlitePool, server_id: &str) -> Result<Vec<CoverState>> {
    Ok(sqlx::query_as("SELECT id, cover_cache_path, has_cover FROM items WHERE server_id = ?").bind(server_id).fetch_all(pool).await?)
}

/// A library's items that can be played: podcasts and e-book-only items (`has_audio = 0`) are
/// left out, so no list in the app shows them.
pub async fn list_for_library(pool: &SqlitePool, server_id: &str, library_id: &str) -> Result<Vec<Item>> {
    let items = sqlx::query_as(
        "SELECT id, server_id, library_id, title, author, narrator, description,
                cover_cache_path, duration_seconds, added_at, synced_at,
                series_name, genres AS genres_json, title_sort, author_sort
         FROM items WHERE server_id = ? AND library_id = ? AND has_audio = 1 ORDER BY added_at DESC",
    )
    .bind(server_id)
    .bind(library_id)
    .fetch_all(pool)
    .await?;
    Ok(items)
}

/// Every item stored for a library, playable or not, with its cached cover path — what sync
/// compares against the server's list to find books the server no longer has.
pub async fn list_ids_for_library(pool: &SqlitePool, server_id: &str, library_id: &str) -> Result<Vec<(String, Option<String>)>> {
    let rows = sqlx::query_as("SELECT id, cover_cache_path FROM items WHERE server_id = ? AND library_id = ?")
        .bind(server_id)
        .bind(library_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Records which of a library's items have no playable audio: `without_audio` get
/// `has_audio = 0`, every other item of the library `1`. One transaction, so a list read
/// mid-update never sees a half-applied state.
pub async fn set_without_audio(pool: &SqlitePool, server_id: &str, library_id: &str, without_audio: &[String]) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE items SET has_audio = 1 WHERE server_id = ? AND library_id = ? AND has_audio = 0")
        .bind(server_id)
        .bind(library_id)
        .execute(&mut *tx)
        .await?;
    for id in without_audio {
        sqlx::query("UPDATE items SET has_audio = 0 WHERE server_id = ? AND library_id = ? AND id = ?")
            .bind(server_id)
            .bind(library_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn set_cover_cache_path(
    pool: &SqlitePool,
    server_id: &str,
    id: &str,
    path: Option<&str>,
) -> Result<()> {
    let result = sqlx::query("UPDATE items SET cover_cache_path = ? WHERE server_id = ? AND id = ?")
        .bind(path)
        .bind(server_id)
        .bind(id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(StorageError::NotFound(format!("item {id} on server {server_id}")));
    }
    Ok(())
}

pub async fn remove(pool: &SqlitePool, server_id: &str, id: &str) -> Result<()> {
    let result = sqlx::query("DELETE FROM items WHERE server_id = ? AND id = ?")
        .bind(server_id)
        .bind(id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(StorageError::NotFound(format!("item {id} on server {server_id}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connect_and_migrate;
    use crate::repo::{libraries, servers};

    async fn pool_with_library() -> (SqlitePool, String, String) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = connect_and_migrate(&tmp.path().join("db.sqlite3"))
            .await
            .unwrap();
        std::mem::forget(tmp);
        let server_id = servers::add(&pool, "https://a.example").await.unwrap();
        libraries::upsert(
            &pool,
            libraries::UpsertLibrary {
                id: "lib-1",
                server_id: &server_id,
                name: "Audiobooks",
                media_type: "book",
                icon: None,
                display_order: 1,
            },
        )
        .await
        .unwrap();
        (pool, server_id, "lib-1".to_string())
    }

    fn item<'a>(server_id: &'a str, library_id: &'a str, id: &'a str, title: &'a str) -> UpsertItem<'a> {
        UpsertItem {
            id,
            server_id,
            library_id,
            title,
            author: Some("Andy Weir"),
            narrator: Some("Ray Porter"),
            description: Some("A lone astronaut..."),
            duration_seconds: 58230.0,
            added_at: Utc::now(),
            series_name: None,
            genres: &[],
        }
    }

    #[tokio::test]
    async fn upsert_then_get_round_trips() {
        let (pool, server_id, library_id) = pool_with_library().await;
        upsert(&pool, item(&server_id, &library_id, "item-1", "Project Hail Mary"))
            .await
            .unwrap();

        let found = get(&pool, &server_id, "item-1").await.unwrap();
        assert_eq!(found.title, "Project Hail Mary");
        assert_eq!(found.author.as_deref(), Some("Andy Weir"));
        assert_eq!(found.cover_cache_path, None);
        assert_eq!(found.series_name, None);
        assert_eq!(found.genres(), Vec::<String>::new());
    }

    #[tokio::test]
    async fn upsert_then_get_round_trips_series_and_genres() {
        let (pool, server_id, library_id) = pool_with_library().await;
        let genres = ["Fantasy".to_string(), "Sci-Fi".to_string()];
        let mut with_series = item(&server_id, &library_id, "item-1", "Wizard's First Rule");
        with_series.series_name = Some("Sword of Truth");
        with_series.genres = &genres;
        upsert(&pool, with_series).await.unwrap();

        let found = get(&pool, &server_id, "item-1").await.unwrap();
        assert_eq!(found.series_name.as_deref(), Some("Sword of Truth"));
        assert_eq!(found.genres(), vec!["Fantasy".to_string(), "Sci-Fi".to_string()]);
    }

    #[tokio::test]
    async fn upsert_twice_updates_in_place() {
        let (pool, server_id, library_id) = pool_with_library().await;
        upsert(&pool, item(&server_id, &library_id, "item-1", "Old Title")).await.unwrap();
        upsert(&pool, item(&server_id, &library_id, "item-1", "New Title")).await.unwrap();

        let items = list_for_library(&pool, &server_id, &library_id).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "New Title");
    }

    #[tokio::test]
    async fn removing_a_library_cascades_to_its_items() {
        let (pool, server_id, library_id) = pool_with_library().await;
        upsert(&pool, item(&server_id, &library_id, "item-1", "Title")).await.unwrap();

        libraries::remove(&pool, &server_id, &library_id).await.unwrap();

        assert!(get(&pool, &server_id, "item-1").await.is_err());
    }

    #[tokio::test]
    async fn upsert_rejects_unknown_library() {
        let (pool, server_id, _library_id) = pool_with_library().await;
        let result = upsert(&pool, item(&server_id, "no-such-library", "item-1", "Title")).await;
        assert!(result.is_err(), "FK to (server_id, library_id) should reject this");
    }

    #[tokio::test]
    async fn set_cover_cache_path_updates_and_can_be_cleared() {
        let (pool, server_id, library_id) = pool_with_library().await;
        upsert(&pool, item(&server_id, &library_id, "item-1", "Title")).await.unwrap();

        set_cover_cache_path(&pool, &server_id, "item-1", Some("/cache/covers/item-1.jpg"))
            .await
            .unwrap();
        assert_eq!(
            get(&pool, &server_id, "item-1").await.unwrap().cover_cache_path,
            Some("/cache/covers/item-1.jpg".to_string())
        );

        set_cover_cache_path(&pool, &server_id, "item-1", None).await.unwrap();
        assert_eq!(get(&pool, &server_id, "item-1").await.unwrap().cover_cache_path, None);
    }

    #[tokio::test]
    async fn list_for_library_orders_newest_first() {
        let (pool, server_id, library_id) = pool_with_library().await;
        let mut older = item(&server_id, &library_id, "old", "Older");
        older.added_at = Utc::now() - chrono::Duration::days(1);
        upsert(&pool, older).await.unwrap();
        upsert(&pool, item(&server_id, &library_id, "new", "Newer")).await.unwrap();

        let items = list_for_library(&pool, &server_id, &library_id).await.unwrap();
        let titles: Vec<_> = items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(titles, vec!["Newer", "Older"]);
    }

    #[tokio::test]
    async fn remove_missing_item_is_not_found() {
        let (pool, server_id, _library_id) = pool_with_library().await;
        let err = remove(&pool, &server_id, "no-such-item").await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    /// A database written by the release before migration 0013 (its tables, a stored item, no
    /// extras columns) opens with the new columns empty: nothing lost, nothing misread.
    #[tokio::test]
    async fn migration_0013_keeps_existing_items_and_leaves_the_extras_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("db.sqlite3");
        let pool = connect_and_migrate(&path).await.unwrap();
        let server_id = servers::add(&pool, "https://a.example").await.unwrap();
        libraries::upsert(&pool, libraries::UpsertLibrary { id: "lib-1", server_id: &server_id, name: "A", media_type: "book", icon: None, display_order: 1 }).await.unwrap();
        upsert(&pool, item(&server_id, "lib-1", "item-1", "Project Hail Mary")).await.unwrap();
        // Take the extras away again, as an older database would have them.
        sqlx::query("UPDATE items SET subtitle = NULL, size_bytes = NULL, has_cover = NULL, title_sort = NULL, author_sort = NULL, tags = '[]'").execute(&pool).await.unwrap();

        let stored = get(&pool, &server_id, "item-1").await.unwrap();
        assert_eq!((stored.title.as_str(), stored.title_sort.clone(), stored.author_sort.clone()), ("Project Hail Mary", None, None));
        assert_eq!(get_extras(&pool, &server_id, "item-1").await.unwrap(), ItemExtras { tags_json: "[]".to_string(), ..Default::default() });
        assert_eq!(list_for_library(&pool, &server_id, "lib-1").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn extras_round_trip_and_are_replaced_by_the_next_write() {
        let (pool, server_id, library_id) = pool_with_library().await;
        upsert(&pool, item(&server_id, &library_id, "item-1", "The Hobbit")).await.unwrap();

        let extras = ItemExtras {
            subtitle: Some("There and Back Again".to_string()),
            size_bytes: Some(340_000_000),
            num_chapters: Some(19),
            tags_json: serde_json::to_string(&["classic", "fantasy"]).unwrap(),
            published_year: Some("1937".to_string()),
            publisher: Some("Allen & Unwin".to_string()),
            language: Some("English".to_string()),
            explicit: Some(false),
            isbn: Some("9780261102217".to_string()),
            asin: None,
            has_cover: Some(true),
            title_sort: Some("Hobbit, The".to_string()),
            author_sort: Some("Tolkien, J.R.R.".to_string()),
        };
        set_extras(&pool, &server_id, "item-1", &extras).await.unwrap();
        assert_eq!(get_extras(&pool, &server_id, "item-1").await.unwrap(), extras);
        assert_eq!(extras.tags(), ["classic", "fantasy"]);
        let item_row = get(&pool, &server_id, "item-1").await.unwrap();
        assert_eq!((item_row.title_sort.as_deref(), item_row.author_sort.as_deref()), (Some("Hobbit, The"), Some("Tolkien, J.R.R.")));
        assert_eq!(list_for_library(&pool, &server_id, &library_id).await.unwrap()[0].title_sort.as_deref(), Some("Hobbit, The"));

        // The basic upsert (every other caller) leaves the extras alone.
        upsert(&pool, item(&server_id, &library_id, "item-1", "The Hobbit")).await.unwrap();
        assert_eq!(get_extras(&pool, &server_id, "item-1").await.unwrap(), extras);

        // A field the server stops sending is cleared by the next sync's write.
        set_extras(&pool, &server_id, "item-1", &ItemExtras { tags_json: "[]".to_string(), ..Default::default() }).await.unwrap();
        assert_eq!(get_extras(&pool, &server_id, "item-1").await.unwrap().subtitle, None);

        // An unknown item is not an error and not created.
        set_extras(&pool, &server_id, "nope", &extras).await.unwrap();
        assert!(matches!(get_extras(&pool, &server_id, "nope").await, Err(StorageError::NotFound(_))));
    }

    #[tokio::test]
    async fn cover_states_come_back_for_every_item_of_the_server_in_one_read() {
        let (pool, server_id, library_id) = pool_with_library().await;
        for id in ["a", "b", "c"] {
            upsert(&pool, item(&server_id, &library_id, id, "T")).await.unwrap();
        }
        set_cover_cache_path(&pool, &server_id, "a", Some("/covers/a.jpg")).await.unwrap();
        set_extras(&pool, &server_id, "b", &ItemExtras { has_cover: Some(false), tags_json: "[]".into(), ..Default::default() }).await.unwrap();
        // Another server's items are not included.
        let other = servers::add(&pool, "https://b.example").await.unwrap();
        libraries::upsert(&pool, libraries::UpsertLibrary { id: "lib-1", server_id: &other, name: "A", media_type: "book", icon: None, display_order: 1 }).await.unwrap();
        upsert(&pool, item(&other, "lib-1", "z", "T")).await.unwrap();

        let mut states = cover_states_for_server(&pool, &server_id).await.unwrap();
        states.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(
            states,
            [
                CoverState { id: "a".into(), cover_cache_path: Some("/covers/a.jpg".into()), has_cover: None },
                CoverState { id: "b".into(), cover_cache_path: None, has_cover: Some(false) },
                CoverState { id: "c".into(), cover_cache_path: None, has_cover: None },
            ]
        );
    }
}
