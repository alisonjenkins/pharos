---
description: "Task list for native book support"
---

# Tasks: native book support

**Input**: Design documents from `/specs/004-books/`

**Prerequisites**: [plan.md](./plan.md), [spec.md](./spec.md), [research.md](./research.md),
[data-model.md](./data-model.md), [contracts/books-http.md](./contracts/books-http.md),
[quickstart.md](./quickstart.md)

**Regenerated 2026-07-29** against the post-`/speckit-analyze` plan. The previous
list predated R9–R12 and had no tasks at all for the decision-site audit, the
classification signal, or `PlaybackInfo`. See §What changed.

**Tests**: INCLUDED. Constitution III is test-first and the plan commits to "each
gate gets a failing test first". Where a test would pass without its
implementation, the task says so and names the disarm step.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: parallelisable — different files, no dependency on an incomplete task
- **[Story]**: US1…US5 from spec.md

## Path Conventions

Single Rust workspace. All paths repo-relative from `/home/ali/git/personal/pharos`.
Every command runs inside the devShell: `nix develop --command …`.

## Commit discipline

Task groups marked **COMMIT** are one atomic commit each, leaving the tree
compiling and revertable alone (CLAUDE.md). The boundaries follow plan.md
§"Delivery order" steps 0–13.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: three pure-Rust readers, and nothing else

- [ ] T001 Add `zip`, `sevenz-rust` and `lopdf` to `[workspace.dependencies]` in `Cargo.toml` and as `*.workspace = true` in `crates/pharos-scanner/Cargo.toml`. Do **not** add an XML crate — `quick-xml` is already a `pharos-scanner` dependency (`crates/pharos-scanner/Cargo.toml:29`) and handles `container.xml`, the OPF and `ComicInfo.xml`
- [ ] T002 Run `nix develop --command just hakari-regen` and commit the regenerated `crates/workspace-hack/Cargo.toml` **together with** `Cargo.lock` (a stale hack crate fails CI's `just hakari-check`; an uncommitted `Cargo.lock` breaks the Nix `buildRustPackage` build) — **COMMIT**
- [ ] T003 Confirm no C dependency crept in: `nix develop --command cargo tree -p pharos-scanner -e no-dev | grep -iE '\-sys$'` names nothing new. This is load-bearing — it is the same constraint that rules out a PDF rasteriser (R11) and a rar reader (R7)

**Checkpoint**: `cargo build --workspace --locked` succeeds, new deps unused.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: every shared gate. This phase is large and that is honest — the
feature is mostly shared infrastructure, and the previous task list pretended
otherwise by labelling `Path` and `/Download` as US1-only.

**⚠️ CRITICAL**: no user story work can begin until this phase is complete.

### Step 0 — audit the decision sites the compiler will NOT find (R10)

- [ ] T004 Write `specs/004-books/kind-decision-audit.md`: enumerate every production site that decides on item kind **without** an exhaustive match — `rg 'matches!\(.*kind|kind == (pharos_core::)?MediaKind' crates/ -g '!*/tests/*'` (34 sites) plus every `match … kind {` block carrying a `_ =>` arm (10 sites), excluding inline `#[cfg(test)]` modules. For each, record file:line, current behaviour, and the intended verdict for `Book` — "behaves like video", "excluded", or "must now branch". Read-only; **no code change**. Do this FIRST: after T005 the tree compiles clean and this list becomes invisible, which is exactly when it stops being auditable. Minimum sites that must appear with a verdict: `dto.rs:1588` (`_ => "Video"` — FR-002's gate), `dto.rs:1592`, `dto.rs:1701`, `dto.rs:1720`, `dto.rs:1880`, `dto.rs:1887`, `metadata/filename.rs:206`, `fs.rs:1022`, `image_cache.rs` (3), `dlna_xml.rs` (3), `tmdb.rs` (2), `tvdb.rs` (2), `items.rs:6119`, `waveform.rs`, `hls.rs`, `trickplay_backfill.rs` — **COMMIT** (the audit document alone)

### Step 1 — core enums

- [ ] T005 Add `Book` to `MediaKind` in `crates/pharos-core/src/lib.rs:1929`, extend `as_str()` → `"book"` and `from_wire()` to accept `"book"` case-insensitively. Fix every exhaustive-match compile error the compiler names, **and apply T004's recorded verdict at each non-exhaustive site in the same commit** — they are part of adding the variant, not a follow-up
- [ ] T006 [P] Add test `book_is_parsed_from_the_wire_discriminator` in `crates/pharos-core/src/lib.rs` tests: `from_wire("Book")`, `from_wire("book")`, `MediaKind::Book.as_str() == "book"`. *Ordering note: a test naming a variant cannot compile before the variant exists, so T005 precedes T006 by necessity, not by preference. Recorded so the V11 ordering does not read as an oversight*
- [ ] T007 Add `Books` to `LibraryKind` in `crates/pharos-core/src/lib.rs:1402`, `collection_type()` → `"books"`, `parse()` accepting `books`/`book`
- [ ] T008 [P] Add test `a_books_library_reports_the_books_collection_type` in `crates/pharos-core/src/lib.rs` tests — **COMMIT** (T005–T008: variants + audited verdicts, no new behaviour)

### Book metadata types

- [ ] T009 Add `BookFormat` (`Epub`, `Pdf`, `Comic`, `Unreadable`) to `crates/pharos-core/src/lib.rs` with `as_str()`/`parse()` for the store discriminator and **`readable_by_client()`** — `!matches!(self, Unreadable)`. This method is the single authority on readability; do **not** add a `READABLE_BY_CLIENT` extension list, which would be a second answer that can drift (D1)
- [ ] T010 [P] Add test `book_format_round_trips_and_knows_what_a_client_can_open` in `crates/pharos-core/src/lib.rs` tests: `as_str`→`parse` round-trips for all four variants, all four `as_str` values are distinct, and `readable_by_client()` is false for `Unreadable` only
- [ ] T011 Add `BookMeta` to `crates/pharos-core/src/lib.rs` with the seven fields from [data-model.md](./data-model.md), deriving `Debug, Clone, PartialEq, Default`. Title, release date and description are **not** here — they are ordinary item fields (R6)
- [ ] T012 Add `pub book: Option<BookMeta>` to `MediaItem` (`crates/pharos-core/src/lib.rs:38`); fix the struct-literal sites the compiler names — **COMMIT**

### Step 2 — store

- [ ] T013 [P] Add `crates/pharos-store-sqlx/migrations/sqlite/0052_book_metadata.sql` — seven nullable columns on `media_items` per data-model.md, no backfill
- [ ] T014 [P] Add the identical `crates/pharos-store-sqlx/migrations/postgres/0052_book_metadata.sql`
- [ ] T015 Extend `MEDIA_COLUMNS` in `crates/pharos-store-sqlx/src/sqlite.rs:16` with the seven columns, assemble `BookMeta` in `MediaRow::into_domain` when `book_format` is non-null, and update every `INSERT`/`UPSERT` on `media_items` for the new placeholder arity
- [ ] T016 Do the same in `crates/pharos-store-sqlx/src/postgres.rs:31` — a separate string constant, not shared with sqlite. Postgres uses `$N` placeholders, so arity errors are silent at compile time
- [ ] T017 Extend `crates/pharos-store-sqlx/src/any.rs` for the book-carrying methods: whichever of `put`, `get`, `list`, `set_artwork` and the query methods changed signature or row shape in T015/T016. Enumerate them from the compile errors rather than guessing — if none changed, say so in the commit message instead of leaving an empty task
- [ ] T018 Add `crates/pharos-store-sqlx/tests/book_roundtrip.rs`: a `MediaItem` with a populated `BookMeta` survives insert→fetch byte-identically; a non-book item round-trips with `book: None`; a book with `series_index: None` round-trips as `None` and not `0`. Runs against BOTH backends the way `tests/backend_conformance.rs` does
- [ ] T019 Run `nix develop --command just test-postgres` — mandatory after any `sqlx::query*` edit: placeholder arity and column names are NOT compile-checked, so a broken query passes `just test` and fails `nix flake check` — **COMMIT**

### Step 3 — `Path` on the DTO (gate 2)

- [ ] T020 [P] Add test `path_is_absent_unless_the_client_asks_for_it` in `crates/pharos-server/tests/items_query_golden.rs`: `GET /Items?IncludeItemTypes=Book` has no `Path` key; `&Fields=CanDownload,Path` yields the real filesystem path; **and `&fields=path` yields it too** (V69 — a camelCase spelling silently ignored would disable the whole feature for that client, a recurring bug class here)
- [ ] T021 Add `Path: Option<String>` to `BaseItemDto` in `crates/pharos-jellyfin-api/src/dto.rs` with `skip_serializing_if = "Option::is_none"` — omitted, not null, per the existing "absent means not requested" convention
- [ ] T022 Populate `Path` in `build_items_page_with_fields` (`crates/pharos-server/src/api/jellyfin/items.rs:4816`) gated on `fields_requests(fields, "Path")` — the existing helper at `items.rs:4874` that keeps `People`/`Studios`/`Tags` off the default payload. Applies to every item kind
- [ ] T023 Apply the same gating to the single-item fetch in `crates/pharos-server/src/api/jellyfin/items.rs` so `GET /Items/{id}?Fields=Path` agrees with the list response
- [ ] T024 Make `fields_requests` (`crates/pharos-server/src/api/jellyfin/items.rs:4874`) match the field name case-insensitively, and confirm the `fields` query parameter itself binds case-insensitively — **COMMIT**

### Step 4 — `GET /Items/{id}/Download` (gate 3)

- [ ] T025 [P] Add `crates/pharos-server/tests/item_download.rs`: `?api_key=<token>` with **no** `Authorization` header returns 200 and the exact bytes; `HEAD` returns `Content-Length` equal to the file size (**not 0**); `Range: bytes=0-99` returns 206 with `Content-Range` and 100 bytes; unknown id 404; no token 401
- [ ] T026 Add `crates/pharos-server/src/api/jellyfin/download.rs` per [contracts/books-http.md](./contracts/books-http.md): resolve id → stored path (no client-supplied path, so V9 has nothing to traverse), `Range` support, `Content-Disposition: attachment; filename="<basename>"`, `Accept-Ranges: bytes`, and the extension→`Content-Type` map. Not restricted to books — real Jellyfin serves any item
- [ ] T027 Return a **sized** body so actix's h1 encoder derives a truthful `Content-Length` on `HEAD` (V113/B166: the encoder takes the header from the body's `BodySize` and discards a hand-set one, so `.finish()` yields `Sized(0)`) — the identical mistake B166 fixed one endpoint over
- [ ] T028 Register `download::register(cfg)` in `crates/pharos-server/src/api/jellyfin/mod.rs:43` — **COMMIT**

### Step 5 — the classification signal, before the branch it measures (R12)

- [ ] T029 [P] Add test `classify_labels_are_distinct_and_stable` in `crates/pharos-scanner/src/book/mod.rs` tests asserting every `BookClassifyVerdict`/`BookClassifyReason` `label()` is distinct and matches the exact strings [quickstart.md](./quickstart.md) §8 queries. Metric labels are a dashboard contract — a renamed label breaks alerts silently
- [ ] T030 Add `BookClassifyVerdict` and `BookClassifyReason` enums with `label()` methods to `crates/pharos-scanner/src/book/mod.rs`. Reasons are bounded: `no_cover_entry`, `unsupported_image_encoding`, `rar_unsupported`, `malformed_container`, `format_unreadable`. The offending *value* goes in the log line beside the counter, never into a label
- [ ] T031 Emit `pharos_book_classify_total{format,verdict,reason}` from the classification path in `crates/pharos-scanner/src/book/mod.rs`. Ships **before** T036's branch exists, per Constitution III — the signal must be readable before the thing it measures lands. This counter is what makes SC-003 (cover rate) and SC-005 (what each file was classified as) answerable by query — **COMMIT**

### Step 6 — extension classification and the probe bypass

- [ ] T032 Add `BOOK_EXTENSIONS` (`epub, pdf, cbz, cbr, cbt, cb7, mobi, azw3`) to `crates/pharos-scanner/src/fs.rs` beside `DEFAULT_EXTENSIONS:20`, and extend `DEFAULT_EXTENSIONS` with it so book files are walked at all
- [ ] T033 [P] Add test `walking_a_book_is_not_the_same_question_as_reading_it` in `crates/pharos-scanner/src/fs.rs` tests: `mobi`/`azw3` are in `BOOK_EXTENSIONS` but `BookFormat::Unreadable.readable_by_client()` is false. Asserts the two questions through the **one** authority, not two lists (D1)
- [ ] T034 Add `read_book_meta(path) -> Option<BookMeta>` to `crates/pharos-scanner/src/book/mod.rs` dispatching on extension, returning `BookFormat::Unreadable` for `mobi`/`azw3` and `None` only when the file cannot be opened at all; emit T031's counter on every path. Register `mod book;` in `crates/pharos-scanner/src/lib.rs`
- [ ] T035 Add test `a_book_path_never_reaches_the_prober` in `crates/pharos-scanner/src/fs.rs` tests: a spy `Prober` counting `probe()` calls, a temp dir holding one `.epub`, assert the count is **0** and that one item was still imported (SC-002, V12 — no ffmpeg needed). **Must be seen red first**: today the epub is not walked at all, so assert the item count too, and confirm the count-0 assertion is load-bearing by temporarily routing the epub through the prober and watching it fail
- [ ] T036 Branch on extension BEFORE the prober in `crates/pharos-scanner/src/fs.rs:988` (`probe_one`): a book path goes to `read_book_meta` and yields `MediaItem { kind: Book, book: Some(..), probe: MediaProbe::default() }`. Comment the rationale: `probe_one` returns `None` on probe failure (V6, `fs.rs:872`), so an epub handed to ffmpeg is an item that never exists. No watcher work needed — `watcher.rs:312` → `update_path` → `probe_put_one` → `probe_one`, verified in R4 — **COMMIT**

### Step 7 — a book offers nothing to play (FR-008, FR-010, SC-004)

- [ ] T037 [P] Add test `a_book_offers_nothing_to_play` in `crates/pharos-server/tests/items_query_golden.rs`: `Type == "Book"`, `MediaType == "Book"`, `RunTimeTicks == 0`, `MediaSources == []`, `MediaStreams == []`, and no `Backdrop`/`Thumb` in `ImageTags`. Empty, **not** absent — R9: array fields are default-empty across pharos because jellyfin-web iterates them without null guards (`dto.rs:420`), so omitting them trades a transcode risk for a client crash
- [ ] T038 Fix `crates/pharos-jellyfin-api/src/dto.rs:1588`: the `match item.kind { Audio => "Audio", _ => "Video" }` wildcard silently makes a Book a video. Return `"Book"`. Also `is_video` at `dto.rs:1592`. **This is the single most likely point of failure in the feature** and the compiler flags neither (R10)
- [ ] T039 Fix the image-tag decision sites in `crates/pharos-jellyfin-api/src/dto.rs`: `has_primary` at `:1880` is `!matches!(kind, Audio) || has_primary_art`, so a book advertises a Primary tag unconditionally and a cover-less book 404s on every grid render (B149 shape). Books get `Primary` only when a cover exists, and never `Backdrop` (`:1887`) or `Thumb` (`:1720`)
- [ ] T040 Emit empty `MediaSources`/`MediaStreams` and `RunTimeTicks: 0` for a book in `crates/pharos-jellyfin-api/src/dto.rs`. Comment why 0 and not null: the field is `u64` (`dto.rs:416`), so null would mean changing it for every item kind (R9)
- [ ] T041 [P] Add test `playbackinfo_offers_no_source_for_a_book` in `crates/pharos-server/tests/` asserting `POST /Items/{book}/PlaybackInfo` returns `MediaSources: []` and no `TranscodingUrl` (FR-010)
- [ ] T042 Return no source for a book from `playback_info` (`crates/pharos-server/src/api/jellyfin/items.rs:2081`, routed at `:105-106`) without entering device-profile evaluation or codec negotiation
- [ ] T043 Confirm `IncludeItemTypes=Book` selects book items through the existing `MediaKind::from_wire` path in `crates/pharos-server/src/api/jellyfin/items.rs`; add the wiring if the type filter does not route through it — **COMMIT**

**Checkpoint**: every shared gate is closed and provably inert. Each user story
below is now genuinely independent — it adds one reader or one presentation
concern and nothing else.

---

## Phase 3: User Story 1 — read an ebook (Priority: P1) 🎯 MVP

**Goal**: an `.epub` in a books library opens in unmodified jellyfin-web and turns a page.

**Independent test**: seed one `.epub`, open it in jellyfin-web, turn a page.

- [ ] T044 [P] [US1] Add an epub fixture under `crates/pharos-scanner/tests/fixtures/` — generate it in-test as a zip holding `META-INF/container.xml` plus an OPF, which keeps a binary blob out of the repo
- [ ] T045 [P] [US1] Add test `an_epubs_opf_metadata_is_read` in `crates/pharos-scanner/src/book/epub.rs` tests: `dc:creator`→author, `dc:publisher`, `calibre:series`/`calibre:series_index`, `dc:identifier`→isbn, and `page_count` stays `None` (epub has no stable page count — deliberate)
- [ ] T046 [P] [US1] Add test `a_malformed_epub_still_imports_the_item` asserting a zip with no `container.xml` yields `BookMeta { format: Epub, .. }` with empty fields rather than `None`, that the item still imports (V6), and that the counter records `malformed_container`
- [ ] T047 [US1] Implement `crates/pharos-scanner/src/book/epub.rs` using the existing `quick-xml`: open the zip, read `META-INF/container.xml` → OPF path → parse the OPF `<metadata>` Dublin Core block. No `unwrap`/`expect` (V17); every error carries the offending value — the zip path and the entry name — never a bare class
- [ ] T048 [US1] Wire `epub` into the `read_book_meta` dispatch in `crates/pharos-scanner/src/book/mod.rs`
- [ ] T049 [US1] Walk [quickstart.md](./quickstart.md) §1–§6 against a real books library: items imported with zero ffmpeg invocations, `CollectionType: "books"`, `Path` gated and spelled both ways, `MediaSources: []` from `PlaybackInfo`, and a truthful `Content-Length` on the `/Download` HEAD
- [ ] T050 [US1] Open the epub in unmodified jellyfin-web via `nix develop --command just compat-playwright-full` and **turn a page** (quickstart §9). If the card opens nothing and the network tab shows no `/Download` request, it is gate 1 or 2 (`MediaType`/`Path`) — check T038 first — **COMMIT**

**Checkpoint**: US1 is independently deliverable and is the MVP.

---

## Phase 4: User Story 2 — read a comic (Priority: P1)

**Goal**: a `.cbz` opens in `comicsPlayer` and pages through as images.

**Independent test**: seed one `.cbz` of known page count, open, page forward.

- [ ] T051 [P] [US2] Add a `.cbz` fixture generated in-test — a zip of two tiny numbered JPEGs — under `crates/pharos-scanner/tests/fixtures/`
- [ ] T052 [P] [US2] Add test `a_comics_page_count_and_comicinfo_are_read` in `crates/pharos-scanner/src/book/comic.rs` tests: `page_count` equals the image-entry count; `ComicInfo.xml` `Series`/`Number`/`Writer` map to `series_name`/`series_index`/`author`
- [ ] T053 [P] [US2] Add test `a_comic_without_comicinfo_still_reports_a_page_count` asserting the archive-only path yields `page_count: Some(n)` with other fields `None`
- [ ] T054 [US2] Implement `crates/pharos-scanner/src/book/comic.rs`: enumerate entries (zip via `zip`, cb7 via `sevenz-rust`), count image entries in name order, parse `ComicInfo.xml` with `quick-xml` when present. One archive open per file
- [ ] T055 [US2] Wire `cbz`/`cbt`/`cb7` into the `read_book_meta` dispatch in `crates/pharos-scanner/src/book/mod.rs`
- [ ] T056 [US2] Classify `.cbr` as `BookFormat::Comic` — it lists, downloads and reads, because libarchive.js unpacks rar client-side — and extract no cover, recording `rar_unsupported` on the counter. **Settled, not deferred** (R7): `unrar` wraps a C library, the same objection that rules out a PDF rasteriser, so `.cbr` is permanently cover-less-but-readable. Counted so the number is visible rather than mysterious
- [ ] T057 [US2] Open the `.cbz` in jellyfin-web and page forward (quickstart §9 step 3) — **COMMIT**

---

## Phase 5: User Story 3 — browse a book library (Priority: P1)

**Goal**: books appear as their own library with covers, authors and series grouping.

**Independent test**: `/UserViews` returns a `books` collection and the grid renders covers.

### The library view

- [ ] T058 [P] [US3] Add test `a_books_library_presents_as_a_books_collection` in `crates/pharos-server/tests/jellyfin_add_library.rs` asserting `GET /UserViews` returns `CollectionType: "books"` for a `kind = "books"` library
- [ ] T059 [US3] Confirm the library-view builder in `crates/pharos-server/src/api/jellyfin/` emits `LibraryKind::Books.collection_type()` rather than a hardcoded set; extend it if the mapping is exhaustive-matched elsewhere

### Covers

- [ ] T060 [P] [US3] Add test `an_epub_cover_is_registered_as_primary_art` in `crates/pharos-scanner/src/book/mod.rs` tests asserting cover bytes go through the store's `set_artwork` and that `has_primary_art` becomes true. **Disarm-verify** by swapping to `put` and watching the flag assertion go red — `put` does not maintain the denormalisation (B155)
- [ ] T061 [US3] Extract the epub cover in `crates/pharos-scanner/src/book/epub.rs`: OPF `<meta name="cover">` → manifest href, else the first image in the spine; record `no_cover_entry` when neither exists
- [ ] T062 [US3] Extract the comic cover in `crates/pharos-scanner/src/book/comic.rs`: first image entry in name order
- [ ] T063 [US3] Write extracted covers through `set_artwork(…, ImageRole::Primary)` from the scan path in `crates/pharos-scanner/src/fs.rs`, so the existing image cache and `has_primary_art` apply unchanged
- [ ] T064 [P] [US3] Add an end-to-end test in `crates/pharos-server/tests/` that a scanned cover-less book advertises no `ImageTags.Primary` and that `GET /Items/{id}/Images/Primary` is consistent with the tag — no advertised-then-404 pair (B149). Complements T037, which asserts the DTO shape; this asserts the scan-to-wire path

### Authors, series and the rest of the metadata

- [ ] T065 [P] [US3] Add test `book_metadata_flows_through_the_existing_resolver` in `crates/pharos-scanner/src/metadata/tests.rs`: the book provider participates in the priority-ordered merge; `dc:date`→release date and `dc:description`→overview land on the **item** and not in `BookMeta` (R6); a file whose metadata has no title falls back to the filename; and `pharos_metadata_field_source_total{field,provider}` records which provider supplied the year (B169's counter, reused — add no new metric)
- [ ] T066 [US3] Add `crates/pharos-scanner/src/metadata/book.rs` implementing `MetadataProvider` over the book file, registered in `crates/pharos-scanner/src/metadata/mod.rs` at **embedded priority (30)** on the existing ladder (nfo 100 > sidecar 50 > embedded 30 > filename 10). Supplies title, author, publisher, series, release date and description
- [ ] T067 [US3] Admit books to the filename provider: `crates/pharos-scanner/src/metadata/filename.rs:206` gates on `matches!(kind, Movie | Episode)`, so a book currently gets no filename-derived title and FR-007 requires that no book is listed untitled. One of T004's audit sites
- [ ] T068 [US3] Project `SeriesName` from `book.series_name` and `IndexNumber` from `book.series_index` in `crates/pharos-jellyfin-api/src/dto.rs`
- [ ] T069 [P] [US3] Add test `books_sort_into_reading_order_within_a_series` in `crates/pharos-server/tests/items_query_golden.rs`: `SortBy=SeriesSortName,SortName` orders three books of one series by `book_series_index`, and a fourth with **no** index sorts last rather than as zero
- [ ] T070 [US3] Support that sort in `crates/pharos-store-sqlx/src/sqlite.rs` and `postgres.rs` using `book_series`/`book_series_index`; re-run `nix develop --command just test-postgres` — **COMMIT**

---

## Phase 6: User Story 4 — read a PDF (Priority: P2)

**Goal**: a `.pdf` opens in `pdfPlayer`.

**Independent test**: seed one `.pdf`, open it, page one renders.

- [ ] T071 [P] [US4] Add test `a_pdfs_info_dictionary_and_page_count_are_read` in `crates/pharos-scanner/src/book/pdf.rs` tests asserting `page_count`, plus title and author from the document info dictionary when present
- [ ] T072 [P] [US4] Add test `a_pdf_cover_comes_out_only_when_page_one_is_already_a_jpeg`: a PDF whose page one is a `DCTDecode` XObject yields cover bytes that are a valid JPEG; a text-first PDF yields **no** cover and records `unsupported_image_encoding`. This is the narrowing R11 settled — asserted so it cannot be quietly widened later
- [ ] T073 [US4] Implement `crates/pharos-scanner/src/book/pdf.rs` with `lopdf`: info dictionary, page count, and page one's embedded image when it is `DCTDecode` (the stream bytes **are** a JPEG, so they go straight to the image cache with no decode step). **No rasterisation** — `lopdf` cannot, and there is no pure-Rust image decoder in the tree; adding either a rasteriser or a decoder breaks single-binary deploy (R11)
- [ ] T074 [US4] Wire `pdf` into the `read_book_meta` dispatch and open the `.pdf` in jellyfin-web, confirming page one renders (quickstart §9 step 4) — **COMMIT**

---

## Phase 7: User Story 5 — resume where I left off (Priority: P3)

**Goal**: reopening a part-read book returns to the last position.

**Independent test**: read a few pages, navigate away, reopen, land in the same place.

- [ ] T075 [P] [US5] Add test `a_books_read_position_round_trips_through_userdata` in `crates/pharos-server/tests/` asserting a progress report for a book persists and returns as `UserData.PlaybackPositionTicks`, and that `RunTimeTicks` stays 0 alongside it
- [ ] T076 [US5] Accept progress reports for book items on the existing playback-reporting path (`crates/pharos-server/src/api/jellyfin/sessions.rs` / `user_data.rs`) — a book must not be rejected for having no media source
- [ ] T077 [US5] Verify by hand per quickstart §10: reopen a part-read book, confirm a non-zero `PlaybackPositionTicks`, **and look at the player UI**. `RunTimeTicks` is 0, so confirm the client renders **no** progress bar rather than a broken, full or NaN one — different outcomes, only one acceptable. Observe it; do not assume it, and do not "fix" it by inventing a runtime (R8) — **COMMIT**

---

## Phase 8: Polish & Cross-Cutting Concerns

- [ ] T078 Append the `Path`-exposure clause to `specs/001-pharos-baseline/invariants.md` at the next free V-number: `Path` is emitted only to an authenticated client AND only when `Fields` requests it, in any spelling; a synthetic path shaped to pass a client-side extension test is forbidden. Never renumber — append
- [ ] T079 Append a second invariant at the next free V-number, generalising T004: **adding a `MediaKind` or `LibraryKind` variant requires auditing the non-exhaustive decision sites, because the compiler does not enumerate them.** This feature found 44 such sites, one of which decided `MediaType`. Cite `specs/004-books/kind-decision-audit.md` as the worked example
- [ ] T080 [P] Append to `specs/001-pharos-baseline/bugs.md` at the next free B-numbers the two silent-failure classes this feature designed around: a missing `Path` making an item unopenable with no error, and a `HEAD` `Content-Length: 0` from an unsized actix body (cross-referencing B166/V113)
- [ ] T081 [P] Confirm `PROBE_SCHEMA_VERSION` in `crates/pharos-core/src/lib.rs` does **not** need a bump — book rows are new and no existing media item's probe output changes. Record the conclusion in the commit message rather than bumping reflexively; a bump re-probes ~13k items
- [ ] T082 Run `nix develop --command just test` and `nix develop --command cargo clippy --workspace --all-targets -- -D warnings`. Pre-commit only runs rustfmt, and a clippy failure silently blocks the image publish
- [ ] T083 Run `nix develop --command just test-postgres` once more after the final `sqlx::query*` edit
- [ ] T084 Deploy and verify by query, not by deploy event: `sum by (verdict) (pharos_book_classify_total)` and `sum by (format) (pharos_book_classify_total)` answer SC-003 and SC-005, and quickstart §3/§5's `Path`/`MediaSources`/`PlaybackInfo` assertions hold against the live server. `/metrics` is on the **main HTTP port**, not 9090 — **COMMIT**

---

## Dependencies

```
Phase 1 (setup: 3 deps)
   └─> Phase 2 (foundational — every shared gate, steps 0–7)
          step 0 audit ──> step 1 enums ──> types ──> store
                                             ├──> step 3 Path
                                             ├──> step 4 /Download
                                             ├──> step 5 classify signal
                                             │       └──> step 6 classify branch
                                             └──> step 7 inertness
          ├─> Phase 3 US1 (epub)      ── MVP
          ├─> Phase 4 US2 (comic)
          ├─> Phase 5 US3 (browse)
          ├─> Phase 6 US4 (pdf)
          └─> Phase 7 US5 (progress)  ── needs any one reader
                 └─> Phase 8 (polish)
```

**Story independence is now real.** The previous task list put `Path` and
`/Download` inside US1 and then had to admit US2/US4 depended on them. Those
gates, plus inertness and the classify branch, are shared by every story, so they
live in Foundational where the dependency is structural rather than a footnote.
After Phase 2, US1–US4 each add exactly one reader and can proceed in any order
or concurrently; US5 needs any one of them.

**Ordering that is not negotiable**: T004 before T005 (the audit is only possible
while `Book` does not exist), and T029–T031 before T036 (the signal ships before
the branch it measures — Constitution III).

## Parallel opportunities

- **Phase 2**: T006 ∥ T008 ∥ T010 (tests in different modules); T013 ∥ T014 (sqlite and postgres migration files); T020 ∥ T025 ∥ T029 (three independent test files, all failing first); T033 alongside T032's implementation; T037 ∥ T041
- **Phase 3**: T044 ∥ T045 ∥ T046
- **Phase 4**: T051 ∥ T052 ∥ T053
- **Phase 5**: T058 ∥ T060 ∥ T064 ∥ T065 ∥ T069 (five independent test files); T061 ∥ T062 (different files)
- **Phase 6**: T071 ∥ T072
- **Phase 8**: T080 ∥ T081
- **Across stories**: once Phase 2 completes, Phases 3–6 are fully concurrent — different files throughout, no shared edit

## Independent test criteria

| Story | Criterion |
|---|---|
| US1 | one `.epub` → opens in unmodified jellyfin-web and turns a page; scan invoked ffmpeg zero times for it |
| US2 | one `.cbz` of known page count → opens in `comicsPlayer`, pages forward, reported `page_count` matches |
| US3 | `/UserViews` returns `CollectionType: "books"`; the grid renders covers with no advertised-then-404 pair; three books of one series sort by index and an index-less fourth sorts last |
| US4 | one `.pdf` → `pdfPlayer` renders page one; a scanned PDF gets a cover, a text-first one gets none and says why |
| US5 | read, navigate away, reopen → non-zero `PlaybackPositionTicks`, `RunTimeTicks` still 0, and **no** broken progress bar |

## Implementation strategy

**MVP = Phase 1 + Phase 2 + Phase 3** (T001–T050): an epub that opens and turns a
page in a stock client, and provably offers nothing to transcode. That is plan.md
steps 0–7. It is the smallest thing worth deploying because it proves all three
player gates plus inertness at once, which is the entire risk of the feature.

**Then, in value order**: US3's library view (T058–T059) so the library stops
looking like a broken video grid; US2 (comics) as the second-largest content
type; US3's covers and metadata; US4 (PDF); US5 (progress).

**Stop-and-reassess point**: if T050 shows the epub failing to open, the network
tab distinguishes the causes — no `/Download` request means `MediaType` or
`Path`, and `MediaType` is the likelier of the two because `dto.rs:1588`'s
wildcard arm defeats it silently and the compiler says nothing. Do not proceed to
Phase 4 until T050 passes; every later story rides on the same gates.

## What changed from the previous task list

Regenerated against the corrected plan. The previous 75 tasks were not wrong so
much as built on two false premises and missing three delivery steps.

| Change | Why |
|---|---|
| **New T004** — audit the 44 non-exhaustive decision sites, before the variant | The plan had claimed the compiler enumerates every site that must decide about books. It does not: 34 `matches!`/`==` sites and 10 wildcard-arm matches, none flagged. `dto.rs:1588`'s `_ => "Video"` decides FR-002's gate (R10) |
| **New T029–T031** — the classify counter, shipped before the branch | Constitution III requires a decision branch record its inputs, verdict and reason. It also makes SC-003 and SC-005 answerable, which nothing previously did (R12/C1) |
| **New T037–T042** — inertness as one group, including `PlaybackInfo` | `PlaybackInfo` had zero tasks despite the contract requiring it (G1), and the old T027/T028 asserted a shape that cannot be produced |
| **T037/T040 reworded** to empty/0 | The old assertion ("no `MediaSources` array", "non-null `RunTimeTicks`") was unsatisfiable, and the obvious fix reopens a documented client-crash class (I1/R9) |
| **T072/T073 rewritten** to embedded-JPEG extraction | The old task carried an unresolved either/or about rasterising. `lopdf` cannot rasterise and no pure-Rust image decoder exists in the tree (I3/R11) |
| **T038/T039/T067 added** as named call sites | `MediaType`, the image tags and the filename-provider gate are all invisible to the compiler (I2/G4) |
| **T009/T033 changed** — one authority on readability | The old `READABLE_BY_CLIENT` list contradicted `BookFormat::Unreadable`, giving two answers that could drift (D1) |
| **T020/T024 extended** to spelling variants | V69; a camelCase `fields=path` silently ignored would disable the feature for that client (I5) |
| **T010 added**, T006/T008 annotated | `BookFormat` had no test at all, and four impl-before-test orderings read as oversights rather than the enum-variant necessity they are (C2) |
| **T017 made concrete** | "any book-carrying method the trait now needs" named no method and no assertion (U1) |
| **T056 settled** | `.cbr` was an open question; closed by the same no-C-library principle as R11 (R7) |
| **T065/T066 extended** | Release date, description and the filename title fallback had no carrier (G2/G4) |
| **T079 added** | The audit's lesson becomes an invariant, so the next `MediaKind` variant does not repeat it |
| **Foundational absorbed `Path`, `/Download`, inertness and the classify branch** | They are shared by every story. The old list called them US1 and then footnoted that US2/US4 depended on them — structure now matches reality |
| **T001 drops the XML crate** | `quick-xml` is already a `pharos-scanner` dependency |
