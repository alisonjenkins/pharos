# Implementation plan: book categories (043)

**Spec**: [spec.md](spec.md). **ADR**: ADR-0129.

## Shape

| Place | Holds |
|---|---|
| `pharos-scanner/src/book/shelf.rs` | The shelf table, `shelves_for(subjects)`, PDF keyword splitting, noise list, and the `pharos_book_category_total` labels and recorder. Pure, table-tested. |
| `book/epub.rs`, `book/kindle.rs`, `book/mobi_exth.rs`, `book/pdf.rs` | Read subjects (`dc:subject`, boko `subjects`, EXTH 105, `/Keywords`). |
| `metadata/book.rs` | Map subjects to shelves (genres) and raw subjects (tags); count and log. |
| `pharos-core` | `BOOK_SCHEMA_VERSION = 3`; `EditableField::Genres` allowed on a `Book`; `GenreStore::item_genre_names`. |
| `pharos-store-sqlx` | `link_item_genres` skips a Genres-locked item; `edit_metadata` writes an item's genre join. Both backends. |
| `pharos-server/src/openlibrary.rs` | Response parsing, the confident-match rule, the rate gate and retry, behind a `Fetch` trait so tests use a fake. |
| `pharos-server/src/book_categories.rs` | The backfill job: eligibility, memo, one pass, spawn. |
| `pharos-server/src/api/jellyfin/metadata_edit.rs`, `services/metadata_edit.rs` | Genres in a book's editor view; the kind check; the as-sent comparison for Jellyfin saves. |

No new UI. The admin editor already renders whatever the server lists as editable, and Genres is a list field there. The Books grid's Genre facet already reads the genre join through `facets`.

## Why genres and not a new column

The Genre facet, the `/Genres` listing and the `ParentId` pivot all read `item_genres`. Storing shelves there makes all three work for books with no new query, no migration and no UI.

## Re-reading existing rows

Books are skipped by `(mtime, size)` unless their stored `BOOK_SCHEMA_VERSION` is older. Bumping it to 3 re-reads every book once, through the book parser only (no ffmpeg).

A rescan relinks genres from the file. A book whose shelf came from Open Library loses it on that rescan, and the next job pass puts it back from the memo with no request. That keeps one writer per source rather than teaching the scan to merge.

## Open Library

- `GET /isbn/{isbn}.json` → `works[0].key` → `GET {key}.json` → `subjects`.
- Else `GET /search.json?title=…&author=…&fields=key,subject,title,author_name&limit=5`.
- Normalised title: lowercase, punctuation to spaces, subtitle after `:` dropped, whitespace collapsed. Author overlap: any word of three or more letters shared.
- 1.1 s between requests (Open Library asks for about one per second from a client that makes many).
- A 404 on the ISBN is a miss for that rung and falls through to the search.

## Gates

- Clippy deny list (V17) holds: no `unwrap`/`expect` outside tests.
- Postgres arm run for the store changes.
- One counter test disarmed once to watch it fail.
