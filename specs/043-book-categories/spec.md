# Feature specification: book categories (043)

**Created**: 2026-11-14T09:10:00Z
**Status**: Draft. Built on the books tidy-up branch.
**Scope**: Every book gets a category (a "shelf") without anyone typing it, and an admin can change it. Shelves are stored as the book's genres, so the Books grid's existing Genre filter narrows by them.
**Decision record**: ADR-0129 (the Open Library lookup).

---

## Problem statement

- **Books carry no genres.** 0% of the library's books have a genre or subject. The scanner never reads an EPUB's `dc:subject`, a Kindle file's EXTH subject or a PDF's `/Keywords`.
- **Folders do not categorise.** About 95% of the book files sit flat in one folder, so a folder name says nothing.
- **The Genre filter is empty for books.** The Books grid already has a Genre facet. It lists the genres present in the listing, and there are none.
- **An admin cannot fix one by hand.** Genres are editable on a show, not on an item.

Rough mix of the library: mostly programming and web, cloud and DevOps, single-board computers and electronics, self-help and productivity, machine learning, security, a little fiction. 82 readable books (epub, pdf), 58 DRM-locked Kindle books whose header gives title, author, publisher and ISBN. About 37 carry an ISBN.

## The approach

1. **Embedded subjects first.** The book's own file is read at scan time:
   - EPUB: every OPF `dc:subject`.
   - Kindle, DRM-free: `boko`'s `subjects`. DRM-locked: every EXTH record 105.
   - PDF: `/Keywords`, only when it reads as a list (two or more short entries split on `,` or `;`).
2. **Open Library second.** A background job looks up a book that has no shelf, or only `Other`:
   - by ISBN: `/isbn/{isbn}.json`, then its first work's `subjects`;
   - else by title and author: `/search.json?title=&author=&fields=key,subject,title,author_name&limit=5`.
   - A search result is used only when its normalised title equals the book's and an author name shares a word with the book's author.
   - Opt-in (`[books].online_subjects`, env `PHAROS_BOOKS_ONLINE`), like LRCLIB lyrics: each lookup sends the book's title, author and ISBN to a third party. Nothing else is sent.
3. **Subjects become shelves.** One pure table maps raw subject strings to a fixed set of shelves: Programming, Web, Cloud & DevOps, Data & AI, Security, Electronics & Making, Maths & Science, Business & Productivity, Self-help, Fiction, Other.
4. **Raw subjects are kept as tags** for embedded subjects (the item's tag join), so nothing read is thrown away.
5. **An admin can edit a book's genres** in the existing metadata editor. The edit locks them, and no scan or lookup overwrites a locked list.

## Requirements

- **FR-001**: The EPUB reader returns every `dc:subject`, trimmed, blanks dropped, in file order.
- **FR-002**: The Kindle reader returns `boko`'s subjects; the EXTH reader returns every record 105.
- **FR-003**: The PDF reader returns `/Keywords` verbatim. The shelf module splits it only when it has two or more entries of at most 40 characters each.
- **FR-004**: The shelf for a subject is the first shelf, in table order, with a whole-word keyword match. A book's shelves are the top two by subject count; the second only when it has at least half the first's count. Subjects that match nothing give `Other`. Known noise subjects (`Accessible book`, `Protected DAISY`, `In library` and similar) are dropped first and give nothing.
- **FR-005**: The book provider returns the shelves as genres and up to 20 raw subjects as tags.
- **FR-006**: `BOOK_SCHEMA_VERSION` is 3, so every book row is re-read once to pick up its subjects.
- **FR-007**: The Open Library job runs only when `[books].online_subjects` is true. It takes books whose genres are empty or only `Other` and not locked.
- **FR-008**: The job asks Open Library at most once per 1.1 s across the process. A 429 or 503 backs off (its `Retry-After`, else 5 s) and retries once. Any other failure counts and moves on to the next book.
- **FR-009**: Each book's answer (subjects or a miss) is memoised in `.book_subjects.json` beside the cache, keyed by ISBN, title and author. A miss is retried after 30 days. A transport error is not memoised.
- **FR-010**: The User-Agent names pharos and its version and says it is a self-hosted media server. It carries no contact address or other personal data.
- **FR-011**: Genres are editable on a book item (not on a film, episode or song) through `/admin/items/{id}/metadata` and Jellyfin's `POST /Items/{id}`. The edit writes the genre join and sets the Genres lock bit.
- **FR-012**: `link_item_genres` leaves an item's genres alone while the Genres lock bit is set. Every writer goes through it, so the scan, the TMDB fill, the probe backfill and the Open Library job all keep a locked list.
- **FR-013**: A Jellyfin save that leaves Genres as `GET /Items` sent them does not edit or lock them.
- **FR-014**: `pharos_book_category_total{source,outcome}` counts each categorisation decision. `source` is `embedded` or `openlibrary`. `outcome` is `shelved`, `other`, `no_subjects`, `no_match`, `unavailable` or `locked`. Labels come from `label()` methods and a test asserts they are distinct.
- **FR-015**: Each decision logs one info line with the item or path, the raw subjects and the shelves chosen. Values go in the log, never in labels.

## Success criteria

- **SC-001**: After one scan and one job pass, every book with embedded subjects or an Open Library match has a shelf. Query: `sum by (source, outcome) (increase(pharos_book_category_total[1d]))`.
- **SC-002**: The Books grid's Genre filter lists only the shelves its books carry.
- **SC-003**: A genre list an admin set survives a full rescan and a job pass.
- **SC-004**: No test touches the network. The Open Library client is tested through a fake fetcher.

## Out of scope

- Showing a book's shelves on its detail page. The item DTO's `Genres` reads the legacy `genre` column, not the join; that is true for films matched online too.
- Raw Open Library subjects as tags. They are logged and memoised only.
- Editing genres on films, episodes or songs.

## Invariants proposed for `invariants.md`

Numbers to be allocated; until then FR-012 and FR-013 stand in.

- A locked genre list is written only by an admin edit (FR-012).
- A book lookup sends only title, author and ISBN, under a User-Agent with no personal data (FR-010).
