//! Backend-independent parts of `MediaStore::put_batch`.

use pharos_core::{DomainResult, MediaId, MediaItem, MediaStore};
use std::collections::HashMap;

/// Drop every item whose id appears again later in the slice, keeping the
/// survivors in their original relative order. This is what N sequential
/// `put`s leave behind, and it keeps one multi-row `ON CONFLICT DO UPDATE`
/// from touching the same row twice (Postgres rejects that outright).
fn dedupe_last_wins(items: &[MediaItem]) -> Vec<&MediaItem> {
    let mut last: HashMap<MediaId, usize> = HashMap::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        last.insert(item.id, i);
    }
    items
        .iter()
        .enumerate()
        .filter(|(i, item)| last.get(&item.id) == Some(i))
        .map(|(_, item)| item)
        .collect()
}

/// The rows a backend writes for `items`: deduped (last wins), then ordered by
/// id. Every transaction then locks and writes its rows in the same order, so
/// two overlapping batches over the same ids cannot deadlock each other (each
/// row's language-index trigger deletes and re-inserts index rows).
pub(crate) fn rows_for_batch(items: &[MediaItem]) -> Vec<&MediaItem> {
    let mut rows = dedupe_last_wins(items);
    rows.sort_unstable_by_key(|item| item.id);
    rows
}

/// Write `rows` one `put` at a time after a batched write failed. Every item
/// is attempted and each failure is logged with the id and path that caused
/// it; the first error is returned.
pub(crate) async fn put_each<S: MediaStore>(store: &S, rows: &[&MediaItem]) -> DomainResult<()> {
    let mut first_err = None;
    for item in rows {
        if let Err(e) = store.put((*item).clone()).await {
            tracing::warn!(
                media.id = item.id,
                path = %item.path.display(),
                error = %e,
                "put_batch: item rejected on per-item retry"
            );
            first_err.get_or_insert(e);
        }
    }
    first_err.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: MediaId, title: &str) -> MediaItem {
        MediaItem {
            id,
            title: title.into(),
            ..Default::default()
        }
    }

    #[test]
    fn dedupe_keeps_the_last_occurrence_in_its_position() {
        let items = [item(1, "a1"), item(2, "b"), item(1, "a2"), item(3, "c")];
        let kept: Vec<_> = dedupe_last_wins(&items)
            .iter()
            .map(|i| (i.id, i.title.as_str()))
            .collect();
        assert_eq!(kept, [(2, "b"), (1, "a2"), (3, "c")]);
    }

    #[test]
    fn batch_rows_are_ordered_by_id_and_the_last_duplicate_wins() {
        let items = [item(3, "c"), item(1, "a1"), item(2, "b"), item(1, "a2")];
        let rows: Vec<_> = rows_for_batch(&items)
            .iter()
            .map(|i| (i.id, i.title.as_str()))
            .collect();
        assert_eq!(rows, [(1, "a2"), (2, "b"), (3, "c")]);
    }

    #[test]
    fn dedupe_of_nothing_is_nothing() {
        assert!(dedupe_last_wins(&[]).is_empty());
    }
}
