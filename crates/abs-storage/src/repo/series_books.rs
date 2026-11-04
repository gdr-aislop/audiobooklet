//! Per-series book membership, cached from `GET /api/libraries/{id}/series` — the only place
//! Audiobookshelf's API exposes a book's real position ("sequence") within a series. A whole
//! library's series list is fetched in one call, so `replace_all_for_library` is the write
//! primitive — same "delete and re-insert in a transaction" reasoning as `repo::chapters::replace_all`,
//! scoped per `library_id` rather than per item this time.

use sqlx::SqlitePool;

use crate::error::Result;

pub struct NewSeriesBook<'a> {
    pub series_id: &'a str,
    pub series_name: &'a str,
    pub item_id: &'a str,
    pub sequence: Option<&'a str>,
}

pub async fn replace_all_for_library(pool: &SqlitePool, server_id: &str, library_id: &str, rows: &[NewSeriesBook<'_>]) -> Result<()> {
    let mut tx = pool.begin().await?;

    sqlx::query("DELETE FROM series_books WHERE server_id = ? AND library_id = ?")
        .bind(server_id)
        .bind(library_id)
        .execute(&mut *tx)
        .await?;

    // A series can list a book this app never stored (an e-book, a podcast, a book the server
    // listed after the items fetch): such a row would break the foreign key and, in one
    // transaction, lose the whole library's series. It's skipped instead. `OR IGNORE` covers a
    // server repeating the same book in one series.
    for row in rows {
        sqlx::query(
            "INSERT OR IGNORE INTO series_books (server_id, library_id, series_id, series_name, item_id, sequence)
             SELECT ?, ?, ?, ?, ?, ?
             WHERE EXISTS (SELECT 1 FROM items WHERE server_id = ? AND id = ?)",
        )
        .bind(server_id)
        .bind(library_id)
        .bind(row.series_id)
        .bind(row.series_name)
        .bind(row.item_id)
        .bind(row.sequence)
        .bind(server_id)
        .bind(row.item_id)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

/// One cached book-in-series row: which series an item belongs to, and its position in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesMembership {
    pub item_id: String,
    pub series_id: String,
    pub series_name: String,
    pub sequence: Option<String>,
}

/// Every cached series membership for a server, across its libraries — what the Library screen
/// groups by. A book in several series has several rows; they come back ordered by series name
/// (then series id), so a caller keeping the first row per item gets a stable choice. Within a
/// series the books come back in the order they were stored, which is the server's order for
/// the series (by their number in it) — the server doesn't send the number itself.
pub async fn list_for_server(pool: &SqlitePool, server_id: &str) -> Result<Vec<SeriesMembership>> {
    let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT item_id, series_id, series_name, sequence FROM series_books
         WHERE server_id = ? ORDER BY series_name COLLATE NOCASE, series_id, rowid",
    )
    .bind(server_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|(item_id, series_id, series_name, sequence)| SeriesMembership { item_id, series_id, series_name, sequence }).collect())
}

/// This item's own series/sequence, plus the series' total book count. `None` if the item was
/// never synced into any series (no series, or the library's series list hasn't been fetched
/// yet).
#[derive(Debug, Clone, PartialEq)]
pub struct SeriesBookInfo {
    pub series_name: String,
    pub sequence: Option<String>,
    pub total_books: i64,
}

/// `total_books` is `max(how many books of this series are synced, the highest `sequence` number
/// among them)`, not a plain count — a library can hold only a sparse subset of a series (e.g.
/// books 2, 5 and 7 of a longer series), and "5 of 7" (7 being the highest known position, not
/// "3", the count of books actually owned) is what a reader expects "book 5" to mean. A
/// non-numeric or fractional `sequence` (e.g. `"5.5"` for an inserted novella) that can't be
/// parsed as a number just doesn't contribute to that maximum; the count alone still applies.
pub async fn find_for_item(pool: &SqlitePool, server_id: &str, item_id: &str) -> Result<Option<SeriesBookInfo>> {
    let Some((series_id, series_name, sequence)): Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT series_id, series_name, sequence FROM series_books WHERE server_id = ? AND item_id = ? LIMIT 1",
    )
    .bind(server_id)
    .bind(item_id)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };

    let sequences: Vec<(Option<String>,)> =
        sqlx::query_as("SELECT sequence FROM series_books WHERE server_id = ? AND series_id = ?")
            .bind(server_id)
            .bind(&series_id)
            .fetch_all(pool)
            .await?;

    let count = sequences.len() as i64;
    let max_known_position = sequences
        .iter()
        .filter_map(|(s,)| s.as_deref())
        .filter_map(|s| s.parse::<f64>().ok())
        .fold(0.0_f64, f64::max);
    let total_books = count.max(max_known_position.ceil() as i64);

    Ok(Some(SeriesBookInfo { series_name, sequence, total_books }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connect_and_migrate;
    use crate::repo::{items, libraries, servers};
    use chrono::Utc;

    async fn pool_with_items(item_ids: &[&str]) -> (SqlitePool, String) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = connect_and_migrate(&tmp.path().join("db.sqlite3")).await.unwrap();
        std::mem::forget(tmp);
        let server_id = servers::add(&pool, "https://a.example").await.unwrap();
        libraries::upsert(
            &pool,
            libraries::UpsertLibrary { id: "lib-1", server_id: &server_id, name: "Audiobooks", media_type: "book", icon: None, display_order: 1 },
        )
        .await
        .unwrap();
        for item_id in item_ids {
            items::upsert(
                &pool,
                items::UpsertItem {
                    id: item_id,
                    server_id: &server_id,
                    library_id: "lib-1",
                    title: "Some Book",
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
        (pool, server_id)
    }

    #[tokio::test]
    async fn replace_all_then_find_for_item_round_trips() {
        let (pool, server_id) = pool_with_items(&["item-1", "item-2", "item-3"]).await;
        replace_all_for_library(
            &pool,
            &server_id,
            "lib-1",
            &[
                NewSeriesBook { series_id: "series-1", series_name: "Foundation", item_id: "item-1", sequence: Some("1") },
                NewSeriesBook { series_id: "series-1", series_name: "Foundation", item_id: "item-2", sequence: Some("2") },
                NewSeriesBook { series_id: "series-1", series_name: "Foundation", item_id: "item-3", sequence: Some("2.5") },
            ],
        )
        .await
        .unwrap();

        let info = find_for_item(&pool, &server_id, "item-2").await.unwrap().unwrap();
        assert_eq!(info.series_name, "Foundation");
        assert_eq!(info.sequence, Some("2".to_string()));
        assert_eq!(info.total_books, 3, "the series has 3 synced books, regardless of which one we looked up");
    }

    /// Regression test: a library that only owns books 2, 5 and 7 of a longer series must report
    /// "5/7" for book 5 — `total_books` is the highest known `sequence` (7), not the count of
    /// synced books (3), which would misleadingly read "5/3".
    #[tokio::test]
    async fn total_books_uses_the_highest_known_sequence_for_a_sparse_series() {
        let (pool, server_id) = pool_with_items(&["item-2", "item-5", "item-7"]).await;
        replace_all_for_library(
            &pool,
            &server_id,
            "lib-1",
            &[
                NewSeriesBook { series_id: "series-1", series_name: "Foundation", item_id: "item-2", sequence: Some("2") },
                NewSeriesBook { series_id: "series-1", series_name: "Foundation", item_id: "item-5", sequence: Some("5") },
                NewSeriesBook { series_id: "series-1", series_name: "Foundation", item_id: "item-7", sequence: Some("7") },
            ],
        )
        .await
        .unwrap();

        let info = find_for_item(&pool, &server_id, "item-5").await.unwrap().unwrap();
        assert_eq!(info.sequence, Some("5".to_string()));
        assert_eq!(info.total_books, 7, "3 books are synced, but book 7 is known to exist, so the total must be 7, not 3");
    }

    #[tokio::test]
    async fn list_for_server_returns_every_membership_of_that_server_only() {
        let (pool, server_id) = pool_with_items(&["item-1", "item-2", "item-3"]).await;
        replace_all_for_library(
            &pool,
            &server_id,
            "lib-1",
            &[
                NewSeriesBook { series_id: "s-b", series_name: "banda", item_id: "item-3", sequence: Some("1") },
                NewSeriesBook { series_id: "s-a", series_name: "Aspirant", item_id: "item-1", sequence: Some("2") },
                NewSeriesBook { series_id: "s-a", series_name: "Aspirant", item_id: "item-2", sequence: None },
            ],
        )
        .await
        .unwrap();
        let other_server = servers::add(&pool, "https://b.example").await.unwrap();

        let memberships = list_for_server(&pool, &server_id).await.unwrap();
        let ids: Vec<&str> = memberships.iter().map(|m| m.item_id.as_str()).collect();
        assert_eq!(ids, ["item-1", "item-2", "item-3"], "ordered by series name (case-insensitively), then item");
        assert_eq!(memberships[0].series_name, "Aspirant");
        assert_eq!(memberships[0].sequence.as_deref(), Some("2"));
        assert_eq!(memberships[1].sequence, None);
        assert!(list_for_server(&pool, &other_server).await.unwrap().is_empty());
    }

    /// The server sorts a series' books by their number but doesn't send it, so the order they
    /// were stored in is the only order there is.
    #[tokio::test]
    async fn list_for_server_keeps_the_stored_book_order_within_a_series() {
        let (pool, server_id) = pool_with_items(&["item-a", "item-b", "item-c"]).await;
        replace_all_for_library(
            &pool,
            &server_id,
            "lib-1",
            &[
                NewSeriesBook { series_id: "s-1", series_name: "Aspirant", item_id: "item-c", sequence: None },
                NewSeriesBook { series_id: "s-1", series_name: "Aspirant", item_id: "item-a", sequence: None },
                NewSeriesBook { series_id: "s-1", series_name: "Aspirant", item_id: "item-b", sequence: None },
            ],
        )
        .await
        .unwrap();

        let ids: Vec<String> = list_for_server(&pool, &server_id).await.unwrap().into_iter().map(|m| m.item_id).collect();
        assert_eq!(ids, ["item-c", "item-a", "item-b"], "not alphabetical: the server's order");
    }

    #[tokio::test]
    async fn replace_all_skips_a_book_that_is_not_stored_instead_of_failing() {
        let (pool, server_id) = pool_with_items(&["item-1"]).await;
        replace_all_for_library(
            &pool,
            &server_id,
            "lib-1",
            &[
                NewSeriesBook { series_id: "series-1", series_name: "Foundation", item_id: "never-stored", sequence: Some("1") },
                NewSeriesBook { series_id: "series-1", series_name: "Foundation", item_id: "item-1", sequence: Some("2") },
                NewSeriesBook { series_id: "series-1", series_name: "Foundation", item_id: "item-1", sequence: Some("2") },
            ],
        )
        .await
        .unwrap();

        let memberships = list_for_server(&pool, &server_id).await.unwrap();
        assert_eq!(memberships.len(), 1, "the unknown book is skipped and the repeated row is stored once");
        assert_eq!(memberships[0].item_id, "item-1");
    }

    #[tokio::test]
    async fn find_for_item_is_none_for_an_unsynced_or_seriesless_item() {
        let (pool, server_id) = pool_with_items(&["item-1"]).await;
        assert!(find_for_item(&pool, &server_id, "item-1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn replace_all_running_twice_does_not_duplicate_or_leave_stale_rows() {
        let (pool, server_id) = pool_with_items(&["item-1", "item-2"]).await;
        replace_all_for_library(
            &pool,
            &server_id,
            "lib-1",
            &[
                NewSeriesBook { series_id: "series-1", series_name: "Foundation", item_id: "item-1", sequence: Some("1") },
                NewSeriesBook { series_id: "series-1", series_name: "Foundation", item_id: "item-2", sequence: Some("2") },
            ],
        )
        .await
        .unwrap();

        // The series shrinks to just one book on the next fetch.
        replace_all_for_library(
            &pool,
            &server_id,
            "lib-1",
            &[NewSeriesBook { series_id: "series-1", series_name: "Foundation", item_id: "item-1", sequence: Some("1") }],
        )
        .await
        .unwrap();

        let info = find_for_item(&pool, &server_id, "item-1").await.unwrap().unwrap();
        assert_eq!(info.total_books, 1, "the stale second row must not linger after a shrunk series list");
        assert!(find_for_item(&pool, &server_id, "item-2").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn removing_an_item_cascades_to_its_series_books_row() {
        let (pool, server_id) = pool_with_items(&["item-1"]).await;
        replace_all_for_library(
            &pool,
            &server_id,
            "lib-1",
            &[NewSeriesBook { series_id: "series-1", series_name: "Foundation", item_id: "item-1", sequence: Some("1") }],
        )
        .await
        .unwrap();

        items::remove(&pool, &server_id, "item-1").await.unwrap();

        assert!(find_for_item(&pool, &server_id, "item-1").await.unwrap().is_none());
    }
}
