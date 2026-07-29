---
description: "Task list for native book support"
---

# Tasks: native book support

**Input**: Design documents from `/specs/004-books/`

**Prerequisites**: [plan.md](./plan.md), [spec.md](./spec.md), [research.md](./research.md),
[data-model.md](./data-model.md), [contracts/books-http.md](./contracts/books-http.md),
[quickstart.md](./quickstart.md)

**Tests**: INCLUDED. Constitution III is test-first and the plan states "each gate
gets a failing test first". Every test task is written to fail before its
implementation task lands — where a test would pass without the change, that is
called out and the task says how to disarm-verify it.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: parallelisable — different files, no dependency on an incomplete task
- **[Story]**: US1…US5 from spec.md

## Path Conventions

Single Rust workspace. All paths repo-relative from `/home/ali/git/personal/pharos`.
Every command runs inside the devShell: `nix develop --command …`.

## Commit discipline

Per CLAUDE.md, each task group marked **COMMIT** below is one atomic commit that
leaves the tree compiling and is revertable alone. The delivery order in plan.md
§"Delivery order" maps to those boundaries.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: pull in the three pure-Rust readers and keep the workspace-hack honest

- [ ] T001 Add `zip`, `sevenz-rust` and `lopdf` to `[workspace.dependencies]` in `Cargo.toml`, and as `zip.workspace = true` / `sevenz-rust.workspace = true` / `lopdf.workspace = true` in `crates/pharos-scanner/Cargo.toml`
- [ ] T002 Run `nix develop --command just hakari-regen` and commit the regenerated `crates/workspace-hack/Cargo.toml` **together with** `Cargo.lock` in the same commit (a stale hack crate fails CI's `just hakari-check`; an uncommitted `Cargo.lock` breaks the Nix `buildRustPackage` build) — **COMMIT**
- [ ] T003 Confirm no runtime dependency was added: `nix develop --command cargo tree -p pharos-scanner -e no-dev | grep -iE 'sys$|cc$'` shows no new C library, preserving single-binary deploy

**Checkpoint**: `cargo build --workspace --locked` succeeds with the new deps unused.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: the type-system and storage changes every story depends on

**⚠️ CRITICAL**: no user story work can begin until this phase is complete

### Core enums (largest blast radius, zero behaviour change)

- [ ] T004 Add `Book` variant to `MediaKind` in `crates/pharos-core/src/lib.rs:1929`, extend `as_str()` → `"book"` and `from_wire()` to accept `"book"` case-insensitively; fix every resulting non-exhaustive-match compile error across `crates/pharos-store-sqlx/`, `crates/pharos-server/`, `crates/pharos-scanner/` with the *no-op* arm for each site (a book is not a movie, not an episode, not audio)
- [ ] T005 [P] Add test `book_is_parsed_from_the_wire_discriminator` in the `mod tests` of `crates/pharos-core/src/lib.rs` asserting `MediaKind::from_wire("Book") == Some(MediaKind::Book)`, `from_wire("book")` likewise, and `MediaKind::Book.as_str() == "book"`
- [ ] T006 Add `Books` variant to `LibraryKind` in `crates/pharos-core/src/lib.rs:1402`, `collection_type()` → `"books"`, and `parse()` accepting `books` and `book`
- [ ] T007 [P] Add test `a_books_library_reports_the_books_collection_type` in `crates/pharos-core/src/lib.rs` tests asserting `LibraryKind::parse("books") == LibraryKind::Books` and `LibraryKind::Books.collection_type() == "books"` — **COMMIT** (T004–T007: variants only, no behaviour)

### Book metadata types

- [ ] T008 Add `BookFormat` enum (`Epub`, `Pdf`, `Comic`, `Unreadable`) to `crates/pharos-core/src/lib.rs` with `as_str()`/`parse()` for the store discriminator, and a doc comment recording that `Unreadable` is `.mobi`/`.azw3` — indexed, not readable by any client reader
- [ ] T009 Add `BookMeta` struct to `crates/pharos-core/src/lib.rs` with the seven fields from [data-model.md](./data-model.md) (`format`, `page_count`, `author`, `publisher`, `series_name`, `series_index`, `isbn`), deriving `Debug, Clone, PartialEq, Default`
- [ ] T010 Add `pub book: Option<BookMeta>` to `MediaItem` in `crates/pharos-core/src/lib.rs:38`, `None` for every non-book item; fix all struct-literal construction sites the compiler names — **COMMIT**

### Store

- [ ] T011 [P] Add `crates/pharos-store-sqlx/migrations/sqlite/0052_book_metadata.sql` adding the seven nullable columns to `media_items` (`book_format TEXT`, `book_page_count INTEGER`, `book_author TEXT`, `book_publisher TEXT`, `book_series TEXT`, `book_series_index INTEGER`, `book_isbn TEXT`) — no backfill, every existing row is NULL
- [ ] T012 [P] Add the identical `crates/pharos-store-sqlx/migrations/postgres/0052_book_metadata.sql`
- [ ] T013 Extend `MEDIA_COLUMNS` in `crates/pharos-store-sqlx/src/sqlite.rs:16` with the seven columns and assemble `BookMeta` in `MediaRow::into_domain` when `book_format` is non-null; update every `INSERT`/`UPSERT` on `media_items` in that file for the new placeholder arity
- [ ] T014 Do the same in `crates/pharos-store-sqlx/src/postgres.rs:31` (`MEDIA_COLUMNS` and every `sqlx::query*` writing `media_items`), noting postgres uses `$N` placeholders so arity errors are silent at compile time
- [ ] T015 Extend `crates/pharos-store-sqlx/src/any.rs` for any book-carrying method the trait now needs
- [ ] T016 Add `crates/pharos-store-sqlx/tests/book_roundtrip.rs` asserting a `MediaItem` with a populated `BookMeta` survives insert→fetch byte-identically, and that a non-book item round-trips with `book: None`; the suite must run against BOTH backends the way `crates/pharos-store-sqlx/tests/backend_conformance.rs` already does
- [ ] T017 Run `nix develop --command just test-postgres` — mandatory after any `sqlx::query*` edit: placeholder arity and column names are NOT compile-checked, so a broken query passes `just test` and fails `nix flake check` — **COMMIT**

### Extension classification and the probe bypass

- [ ] T018 Add `BOOK_EXTENSIONS` (`epub, pdf, cbz, cbr, cbt, cb7, mobi, azw3`) and `READABLE_BY_CLIENT` (`epub, pdf, cbz, cbr, cbt, cb7`) consts to `crates/pharos-scanner/src/fs.rs` beside `DEFAULT_EXTENSIONS:20`, and extend `DEFAULT_EXTENSIONS` with `BOOK_EXTENSIONS` so book files are walked at all; add a comment on why the two lists are separate ("pharos indexes it" ≠ "a client can open it")
- [ ] T019 [P] Add test `the_two_book_extension_sets_are_deliberately_different` in `crates/pharos-scanner/src/fs.rs` tests asserting `mobi`/`azw3` are in `BOOK_EXTENSIONS` but NOT in `READABLE_BY_CLIENT`, and that every `READABLE_BY_CLIENT` entry is also in `BOOK_EXTENSIONS`
- [ ] T020 Add `crates/pharos-scanner/src/book/mod.rs` with `read_book_meta(path) -> Option<BookMeta>` dispatching on extension, returning `BookFormat::Unreadable` metadata for `mobi`/`azw3` and `None` only when the file cannot be opened at all; register `mod book;` in `crates/pharos-scanner/src/lib.rs`
- [ ] T021 Add test `a_book_path_never_reaches_the_prober` in `crates/pharos-scanner/src/fs.rs` tests: a spy `Prober` impl that counts `probe()` calls, a temp dir holding one `.epub`, assert the count is **0** after a scan and that one item was still imported (SC-002). Satisfies V12 — no ffmpeg needed. **This is the test that must fail first**: today the epub is not even walked, so assert the item count too, and confirm the count-0 assertion is load-bearing by temporarily routing the epub through the prober
- [ ] T022 Branch on extension BEFORE the prober in `crates/pharos-scanner/src/fs.rs:988` (`probe_one`): a book path goes to `read_book_meta` and yields `MediaItem { book: Some(..), probe: MediaProbe::default(), kind: MediaKind::Book }`; a media path is unchanged. Rationale in the code comment: `probe_one` returns `None` on probe failure (V6), so an epub handed to ffmpeg is an item that never exists — **COMMIT**

**Checkpoint**: books are walked, classified, stored and round-trip — but no client can see or open one yet.

---

## Phase 3: User Story 1 — read an ebook (Priority: P1) 🎯 MVP

**Goal**: an `.epub` in a books library opens in unmodified jellyfin-web and turns a page.

**Independent test**: seed one `.epub`, open it in jellyfin-web, turn a page
([quickstart.md](./quickstart.md) §6 step 2).

### Gate 1 + gate 2 — the DTO

- [ ] T023 [P] [US1] Add test `path_is_absent_unless_the_client_asks_for_it` in `crates/pharos-server/tests/items_query_golden.rs` asserting `GET /Items?IncludeItemTypes=Book` has no `Path` key, and `GET /Items?IncludeItemTypes=Book&Fields=CanDownload,Path` has a `Path` equal to the item's real filesystem path
- [ ] T024 [US1] Add `Path: Option<String>` to `BaseItemDto` in `crates/pharos-jellyfin-api/src/dto.rs` with `skip_serializing_if = "Option::is_none"` (omitted, not null, per the existing "absent means not requested" convention) and PascalCase rename
- [ ] T025 [US1] Populate `Path` in `build_items_page_with_fields` in `crates/pharos-server/src/api/jellyfin/items.rs:4816` gated on `fields_requests(fields, "Path")` — the existing helper at `items.rs:4874`, the same mechanism that keeps `People`/`Studios`/`Tags` off the default payload. Applies to every item kind, not just books
- [ ] T026 [US1] Apply the same `Path` gating to the single-item fetch path in `crates/pharos-server/src/api/jellyfin/items.rs` so `GET /Items/{id}?Fields=Path` agrees with the list response
- [ ] T027 [P] [US1] Add test `a_book_carries_no_technical_media_fields` in `crates/pharos-server/tests/items_query_golden.rs` asserting a book item has `Type == "Book"`, `MediaType == "Book"`, `RunTimeTicks == null`, and **no** `MediaSources`/`MediaStreams` keys (SC-004)
- [ ] T028 [US1] Project a book in the DTO builder in `crates/pharos-server/src/api/jellyfin/items.rs`: `Type`/`MediaType` both `"Book"`, `CanDownload: true`, `RunTimeTicks` null, `MediaSources`/`MediaStreams` omitted. Comment why `RunTimeTicks` stays null (R8 — no time axis; a fabricated runtime is the failure to avoid)
- [ ] T029 [US1] Verify `IncludeItemTypes=Book` selects book items through the existing `MediaKind::from_wire` path in `crates/pharos-server/src/api/jellyfin/items.rs`; add the wiring if the type filter does not route through it — **COMMIT** (T023–T029: `Path` + book projection)

### Gate 3 — the bytes

- [ ] T030 [P] [US1] Add `crates/pharos-server/tests/item_download.rs` asserting: `GET /Items/{id}/Download?api_key=<token>` with **no** `Authorization` header returns 200 and the exact file bytes; `HEAD` returns `Content-Length` equal to the file size (**not 0**); `Range: bytes=0-99` returns 206 with `Content-Range` and 100 bytes; an unknown id returns 404; no token returns 401
- [ ] T031 [US1] Add `crates/pharos-server/src/api/jellyfin/download.rs` implementing `GET /Items/{itemId}/Download` per [contracts/books-http.md](./contracts/books-http.md): resolve id → stored path (no client-supplied path, so V9 has nothing to traverse), `Range` support, `Content-Disposition: attachment; filename="<basename>"`, `Accept-Ranges: bytes`, and the extension→`Content-Type` map (`application/epub+zip`, `application/pdf`, `application/vnd.comicbook+zip`, `application/vnd.comicbook-rar`, else `application/octet-stream`). Not restricted to books — real Jellyfin serves any item
- [ ] T032 [US1] Return a **sized** body so actix's h1 encoder derives a truthful `Content-Length` on `HEAD` (V113/B166: the encoder takes the header from the body's `BodySize` and discards a hand-set one, so `.finish()` yields `Sized(0)`) — this is the identical mistake B166 fixed one endpoint over
- [ ] T033 [US1] Register `download::register(cfg)` in `crates/pharos-server/src/api/jellyfin/mod.rs:43` — **COMMIT** (T030–T033: `/Download`)

### The epub reader

- [ ] T034 [P] [US1] Add a freely-licensed `.epub` fixture under `crates/pharos-scanner/tests/fixtures/` (or generate one in-test as a zip with `META-INF/container.xml` + an OPF, which keeps the repo free of a binary blob)
- [ ] T035 [P] [US1] Add test `an_epubs_opf_metadata_is_read` in `crates/pharos-scanner/src/book/epub.rs` tests asserting title, `dc:creator` → `author`, `dc:publisher`, `calibre:series`/`calibre:series_index`, `dc:identifier` → `isbn` are extracted, and `page_count` stays `None` (epub has no stable page count — deliberate, per data-model.md)
- [ ] T036 [P] [US1] Add test `a_malformed_epub_still_imports_the_item` asserting a zip with no `container.xml` yields a `BookMeta { format: Epub, .. }` with empty fields rather than `None`, and that the item is still imported (V6 isolation)
- [ ] T037 [US1] Implement `crates/pharos-scanner/src/book/epub.rs`: open the zip, read `META-INF/container.xml` → OPF path → parse the OPF `<metadata>` Dublin Core block; no `unwrap`/`expect` (V17) and every error carries the offending value (path + entry name), never a bare class
- [ ] T038 [US1] Wire `epub` into the `read_book_meta` dispatch in `crates/pharos-scanner/src/book/mod.rs` — **COMMIT** (T034–T038: epub reader)

### US1 acceptance

- [ ] T039 [US1] Configure a books library (`kind = "books"`) per [quickstart.md](./quickstart.md), scan, and walk quickstart §1–§4: three items imported with zero ffmpeg invocations, `CollectionType: "books"`, `Path` present only with `Fields=Path`, and a truthful `Content-Length` on the `/Download` HEAD
- [ ] T040 [US1] Open the epub in unmodified jellyfin-web via `nix develop --command just compat-playwright-full` and **turn a page** (quickstart §6). If the card opens nothing and the network tab shows no `/Download` request, the fault is gate 1 or 2 (`MediaType`/`Path`), not the bytes

**Checkpoint**: US1 is independently deliverable and is the MVP.

---

## Phase 4: User Story 2 — read a comic (Priority: P1)

**Goal**: a `.cbz` opens in `comicsPlayer` and pages through as images.

**Independent test**: seed one `.cbz` of known page count, open, page forward.

- [ ] T041 [P] [US2] Add a `.cbz` fixture (generated in-test: a zip of two tiny numbered JPEGs) under `crates/pharos-scanner/tests/fixtures/`
- [ ] T042 [P] [US2] Add test `a_comics_page_count_and_comicinfo_are_read` in `crates/pharos-scanner/src/book/comic.rs` tests asserting `page_count` equals the image-entry count and that `ComicInfo.xml` `Series`/`Number`/`Writer` map to `series_name`/`series_index`/`author`
- [ ] T043 [P] [US2] Add test `a_comic_without_comicinfo_still_reports_a_page_count` asserting the archive-only path yields `page_count: Some(n)` with the other fields `None`
- [ ] T044 [US2] Implement `crates/pharos-scanner/src/book/comic.rs`: enumerate archive entries (zip via `zip`, cb7 via `sevenz-rust`), count image entries in name order, parse `ComicInfo.xml` when present. One archive open per file
- [ ] T045 [US2] Wire `cbz`/`cbt`/`cb7` into the `read_book_meta` dispatch in `crates/pharos-scanner/src/book/mod.rs`
- [ ] T046 [US2] Handle `.cbr` explicitly: classify as `BookFormat::Comic` so it lists, downloads and reads (libarchive.js unpacks rar client-side), but extract **no** cover — and log at debug WHY, naming the extension. R7 is an open non-gating gap: unrar is not in the devShell. Advertising a cover that 404s is the B149 failure shape
- [ ] T047 [US2] Open the `.cbz` in jellyfin-web and page forward (quickstart §6 step 3) — **COMMIT**

---

## Phase 5: User Story 3 — browse a book library (Priority: P1)

**Goal**: books appear as their own library with covers, authors and series grouping.

**Independent test**: `/UserViews` returns a `books` collection and the grid renders covers.

- [ ] T048 [P] [US3] Add test `a_books_library_presents_as_a_books_collection` in `crates/pharos-server/tests/jellyfin_add_library.rs` asserting `GET /UserViews` returns `{"CollectionType":"books"}` for a `kind = "books"` library (fails today: `LibraryKind::parse` did not know `books` before T006, and jellyfin-web would render a video grid)
- [ ] T049 [US3] Confirm the library-view builder in `crates/pharos-server/src/api/jellyfin/` emits `LibraryKind::Books.collection_type()` rather than a hardcoded set; extend it if the mapping is exhaustive-matched elsewhere

### Covers

- [ ] T050 [P] [US3] Add test `an_epub_cover_is_extracted_and_registered_as_primary_art` in `crates/pharos-scanner/src/book/mod.rs` tests asserting cover bytes are written through the store's `set_artwork` and that `has_primary_art` becomes true — **not** `put`, which does not maintain the denormalisation (B155). Disarm-verify by swapping to `put` and watching the flag assertion go red
- [ ] T051 [US3] Extract the epub cover in `crates/pharos-scanner/src/book/epub.rs`: OPF `<meta name="cover">` → manifest href; fall back to the first image in the spine
- [ ] T052 [US3] Extract the comic cover in `crates/pharos-scanner/src/book/comic.rs`: first image entry in name order
- [ ] T053 [US3] Write extracted covers through `set_artwork(…, ImageRole::Primary)` from the scan path in `crates/pharos-scanner/src/fs.rs`, so the existing on-disk image cache and `has_primary_art` apply unchanged
- [ ] T054 [P] [US3] Add test `a_book_with_no_cover_advertises_no_primary_image_tag` asserting `ImageTags.Primary` is absent when extraction found nothing — the B149 shape is advertising an image that 404s

### Authors and series

- [ ] T055 [P] [US3] Add test `book_metadata_flows_through_the_existing_resolver` in `crates/pharos-scanner/src/metadata/tests.rs` asserting a book provider participates in the priority-ordered merge and that `pharos_metadata_field_source_total{field,provider}` records which provider supplied the year (B169's counter, reused — add no new metric)
- [ ] T056 [US3] Add `crates/pharos-scanner/src/metadata/book.rs` implementing `MetadataProvider` over `BookMeta`, registered in `crates/pharos-scanner/src/metadata/mod.rs` at a priority consistent with the existing ladder (nfo 100 > sidecar 50 > embedded 30 > filename 10 — book-file metadata is embedded-class, so 30)
- [ ] T057 [US3] Project `SeriesName` from `book.series_name` and `IndexNumber` from `book.series_index` in the DTO builder in `crates/pharos-server/src/api/jellyfin/items.rs`
- [ ] T058 [P] [US3] Add test `books_sort_into_reading_order_within_a_series` in `crates/pharos-server/tests/items_query_golden.rs` asserting `SortBy=SeriesSortName,SortName` orders three books of one series by `book_series_index`
- [ ] T059 [US3] Support that sort in the query layer (`crates/pharos-store-sqlx/src/sqlite.rs` + `postgres.rs`) using `book_series` and `book_series_index`; re-run `just test-postgres` — **COMMIT**

---

## Phase 6: User Story 4 — read a PDF (Priority: P2)

**Goal**: a `.pdf` opens in `pdfPlayer`.

**Independent test**: seed one `.pdf`, open it, page 1 renders.

- [ ] T060 [P] [US4] Add test `a_pdfs_info_dictionary_and_page_count_are_read` in `crates/pharos-scanner/src/book/pdf.rs` tests asserting `page_count`, and title/author from the document info dictionary when present
- [ ] T061 [US4] Implement `crates/pharos-scanner/src/book/pdf.rs` reading the info dictionary and page count via `lopdf`; no rendering for delivery — `pdfPlayer` renders client-side (R1)
- [ ] T062 [US4] Wire `pdf` into the `read_book_meta` dispatch in `crates/pharos-scanner/src/book/mod.rs`
- [ ] T063 [US4] Extract page one as the cover through `set_artwork`, or record in the code comment that PDF cover extraction is deferred if `lopdf` cannot rasterise without a renderer — state which, honestly, rather than advertising a cover that 404s
- [ ] T064 [US4] Open the `.pdf` in jellyfin-web and confirm page 1 renders (quickstart §6 step 4) — **COMMIT**

---

## Phase 7: User Story 5 — resume where I left off (Priority: P3)

**Goal**: reopening a part-read book returns to the last position.

**Independent test**: read a few pages, navigate away, reopen, land in the same place.

- [ ] T065 [P] [US5] Add test `a_books_read_position_round_trips_through_userdata` in `crates/pharos-server/tests/` asserting a progress report for a book item persists and is returned as `UserData.PlaybackPositionTicks`, and that `RunTimeTicks` remains null alongside it
- [ ] T066 [US5] Accept progress reports for book items on the existing playback-reporting path in `crates/pharos-server/src/api/jellyfin/sessions.rs` / `user_data.rs` — a book must not be rejected for having no media source
- [ ] T067 [US5] Add a comment at the `RunTimeTicks` projection site recording that no progress BAR will render (position/runtime is undefined) and that this must NOT be "fixed" by inventing a runtime (R8)
- [ ] T068 [US5] Verify by hand per quickstart §7: reopen a part-read book and confirm a non-zero `PlaybackPositionTicks` — **COMMIT**

---

## Phase 8: Polish & Cross-Cutting Concerns

- [ ] T069 Append the `Path`-exposure clause to `specs/001-pharos-baseline/invariants.md` as the next free V-number: `Path` is emitted only to an authenticated client AND only when `Fields` requests it; a synthetic path shaped to pass a client-side extension test is forbidden. Never renumber — append
- [ ] T070 [P] Append to `specs/001-pharos-baseline/bugs.md` the two silent-failure classes this feature had to design around, at the next free B-numbers: a missing `Path` making an item silently unopenable, and a `HEAD` `Content-Length: 0` from an unsized actix body (cross-referencing B166/V113)
- [ ] T071 [P] Update `crates/pharos-core/src/lib.rs` `PROBE_SCHEMA_VERSION` **only if** book classification changes stored probe output for existing media items — it should not, since book rows are new; confirm and record the conclusion rather than bumping reflexively (a bump re-probes ~13k items)
- [ ] T072 Resolve or re-record R7 (`.cbr` cover extraction) in `specs/004-books/research.md` once a real `.cbr` appears in the library; until then it stays open and non-gating
- [ ] T073 Run `nix develop --command just test` (full workspace) and `nix develop --command cargo clippy --workspace --all-targets -- -D warnings` — pre-commit only runs rustfmt, and a clippy failure silently blocks the image publish
- [ ] T074 Run `nix develop --command just test-postgres` once more after the final `sqlx::query*` edit
- [ ] T075 Deploy and verify by query, not by deploy event: `sum by (provider) (pharos_metadata_field_source_total{field="production_year"})` shows the book provider, and the quickstart §3 `Path`/`MediaSources` assertions hold against the live server — **COMMIT**

---

## Dependencies

```
Phase 1 (setup)
   └─> Phase 2 (foundational: enums, BookMeta, store, probe bypass)
          ├─> Phase 3 US1 (epub)  ── MVP, independently shippable
          ├─> Phase 4 US2 (comic) ── needs T024–T033 from US1 (Path + /Download)
          ├─> Phase 5 US3 (browse)── needs T006/T007 only; covers need T037/T044
          ├─> Phase 6 US4 (pdf)   ── needs T024–T033 from US1
          └─> Phase 7 US5 (progress) ── needs a readable book, so any of US1/US2/US4
                 └─> Phase 8 (polish)
```

**Honest note on story independence**: US2 and US4 are *not* fully independent of
US1 — `Path` (T024–T026) and `/Download` (T030–T033) are shared gates that every
reader needs. They sit in US1 because US1 is the MVP and they must ship first;
they are shared infrastructure in all but name. US3's library-view work (T048–T049)
and US5 are genuinely independent given Phase 2.

**Within Phase 2**: T004–T007 (enums) → T008–T010 (types) → T011–T017 (store) →
T018–T022 (classification). Each group is a commit boundary.

## Parallel opportunities

- **Phase 2**: T005 ∥ T007 (tests in different modules); T011 ∥ T012 (sqlite and postgres migration files); T019 alongside T018's implementation
- **Phase 3**: T023 ∥ T027 ∥ T030 (three test files, all failing first); T034 ∥ T035 ∥ T036 (fixture + two reader tests)
- **Phase 4**: T041 ∥ T042 ∥ T043
- **Phase 5**: T048 ∥ T050 ∥ T054 ∥ T055 ∥ T058 (five independent test files); T051 ∥ T052 (epub and comic cover extraction touch different files)
- **Phase 8**: T070 ∥ T071
- **Across stories**: once Phase 3's T033 lands, US2 (Phase 4), US4 (Phase 6) and US3's metadata half (T055–T059) can proceed concurrently — different files throughout

## Independent test criteria

| Story | Criterion |
|---|---|
| US1 | one `.epub` seeded → opens in unmodified jellyfin-web and turns a page; scan invoked ffmpeg zero times for it |
| US2 | one `.cbz` of known page count → opens in `comicsPlayer`, pages forward, reported `page_count` matches |
| US3 | `/UserViews` returns `CollectionType: "books"`; the grid renders covers; three books of one series sort by index |
| US4 | one `.pdf` → `pdfPlayer` renders page 1 |
| US5 | read, navigate away, reopen → non-zero `UserData.PlaybackPositionTicks`, `RunTimeTicks` still null |

## Implementation strategy

**MVP = Phase 1 + Phase 2 + Phase 3** (T001–T040): an epub that opens and turns a
page in a stock client. That is plan.md's steps 1–5 and it is the smallest thing
worth deploying — it proves all three player gates at once, which is the entire
risk of this feature.

**Then, in value order**: Phase 5's library-view half (T048–T049) so the library
stops looking like a broken video grid; Phase 4 (comics) as the second-largest
content type; Phase 5's covers and metadata; Phase 6 (PDF); Phase 7 (progress).

**Stop-and-reassess point**: if T040 shows the epub failing to open, the fault is
one of three gates and the network tab distinguishes them — no `/Download` request
means `MediaType` or `Path`, a failing `/Download` means the bytes. Do not proceed
to Phase 4 until T040 passes, because every later story depends on the same gates.

## Format validation

All 75 tasks carry a checkbox, a sequential `T0NN` id, a `[P]` marker where
parallelisable, a `[USn]` label in every user-story phase (and none in Setup,
Foundational or Polish), and an explicit file path or command.
