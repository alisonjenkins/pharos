# Tasks: book categories (043)

**Input**: [spec.md](spec.md), [plan.md](plan.md), ADR-0129. Ids are local to this spec (`T043-NN`). Every code task is test first. No real book titles, authors, ISBNs or paths in code, tests or fixtures.

## Slice 1 - Embedded subjects

- [x] T043-01 `book/shelf.rs`: the shelf table, noise list, PDF keyword split, `shelves_for`, and the counter labels with a distinctness test (FR-003, FR-004, FR-014).
- [x] T043-02 EPUB `dc:subject` (FR-001).
- [x] T043-03 Kindle subjects: boko `subjects` and EXTH 105 (FR-002).
- [x] T043-04 PDF `/Keywords` (FR-003).
- [x] T043-05 The book provider returns shelves as genres and raw subjects as tags; counts and logs each book (FR-005, FR-014, FR-015). Disarm the counter once and watch its test fail.
- [x] T043-06 `BOOK_SCHEMA_VERSION` 2 → 3 (FR-006).

## Slice 2 - Admin edit and the lock

- [x] T043-07 `GenreStore::item_genre_names` on every store.
- [x] T043-08 Store: an item edit writes the genre join; `link_item_genres` keeps a locked list (FR-011, FR-012). Conformance test on both backends.
- [x] T043-09 Core: Genres editable on a book only (FR-011).
- [x] T043-10 Server: a book's editor view lists its genres; a Jellyfin save that leaves Genres as sent does not lock them (FR-011, FR-013).

## Slice 3 - Open Library

- [x] T043-11 `openlibrary.rs`: parse the ISBN, work and search answers from fixture JSON; the confident-match rule; the rate gate; retry on 429/503 through a fake fetcher (FR-008, FR-010).
- [x] T043-12 `[books].online_subjects` and `PHAROS_BOOKS_ONLINE` (FR-007).
- [x] T043-13 `book_categories.rs`: eligibility, memo, one pass with a fake client (match, miss, throttle, HTTP error, locked), spawn when enabled (FR-007, FR-009, FR-014, FR-015).
- [x] T043-14 The Books library index lists only its books' shelves as genres (SC-002).
