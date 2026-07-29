# Phase 1 data model: native book support

## Changed enums

### `pharos_core::MediaKind` (+1 variant)

```
Movie | Episode | Audio | Book
```

- `as_str()` → `"book"` (the store discriminator).
- `from_wire()` accepts `"book"` case-insensitively (T88(d): the single
  canonical parser for the wire `Type` discriminator).

Exhaustively matched across the store, DTO builders, scanner and query layers,
so the compiler enumerates every site that must now decide about books. That is
the point of adding a variant rather than a boolean.

### `pharos_core::LibraryKind` (+1 variant)

```
Movies | TvShows | Music | Books | Mixed
```

- `collection_type()` → `"books"` — a token the deployed jellyfin-web already
  recognises.
- `parse()` accepts `books`, `book`.

## New: `BookMeta`

Format-specific facts read from the file at scan time. Lives on the item the way
`MediaProbe` does for media, and is `Default` for a non-book item.

| Field | Type | Source | Notes |
|---|---|---|---|
| `format` | `BookFormat` | extension | Epub, Pdf, Comic, Unreadable |
| `page_count` | `Option<u32>` | comic entry count / pdf page count | epub has no stable page count — stays `None`, deliberately |
| `author` | `Option<String>` | epub `dc:creator`, ComicInfo `Writer` | |
| `publisher` | `Option<String>` | epub `dc:publisher` | |
| `series_name` | `Option<String>` | `calibre:series`, ComicInfo `Series` | |
| `series_index` | `Option<u32>` | `calibre:series_index`, ComicInfo `Number` | drives sort within a series |
| `isbn` | `Option<String>` | epub `dc:identifier` | |

`BookFormat::Unreadable` is explicit and load-bearing: `.mobi`/`.azw3` are
browsable but have no client reader (spec Assumptions). Modelling that as a
variant means the DTO builder cannot forget it, rather than it being implied by
an extension list somewhere else.

### Why not reuse `MediaProbe`?

`MediaProbe` is stream-shaped (codecs, pixel format, frame rate, tracks). Nesting
book facts inside it would put "author" beside "pix_fmt" and would make SC-004
(no technical fields on a book) unstateable. Separate struct, `Default` when
absent — same pattern `series: Option<SeriesInfo>` already uses for episodes.

## Changed: `MediaItem`

```
+ pub book: Option<BookMeta>,
```

`None` for every non-book item. `MediaItem.probe` stays as-is and is
`MediaProbe::default()` for a book — already documented as the "probe failure or
pre-ffprobe scan still yields a row" case, so no semantics change.

## Store

New nullable columns on `media_items`, one migration, no backfill (every existing
row is `NULL`):

```
book_format      TEXT NULL
book_page_count  INTEGER NULL
book_author      TEXT NULL
book_publisher   TEXT NULL
book_series      TEXT NULL
book_series_index INTEGER NULL
book_isbn        TEXT NULL
```

Both backends (sqlite + postgres). Per CLAUDE.md, any `sqlx::query*` change means
`just test-postgres` — placeholder arity is not compile-checked, and a broken
query passes `just test` and fails `nix flake check`.

`MEDIA_COLUMNS` gains the seven columns; `MediaRow::into_domain` assembles
`BookMeta` when `book_format` is non-null.

## DTO projection

| Wire field | Value for a book |
|---|---|
| `Type` | `"Book"` |
| `MediaType` | `"Book"` |
| `Path` | real path — **only when `Fields` contains `Path`** |
| `CanDownload` | `true` |
| `RunTimeTicks` | `null` (R8 — even though position is tracked) |
| `MediaSources` / `MediaStreams` | absent (R9) |
| `SeriesName` / `IndexNumber` | from `book.series_name` / `series_index` |
| `ProductionYear` / `PremiereDate` | from the metadata provider, as for any item |
| `ImageTags.Primary` | present iff a cover was extracted |
| `UserData.PlaybackPositionTicks` | reader position (R8) |

`Path` reuses the existing `Fields`-gating helper family in `items.rs` rather
than a new mechanism.

## Cover artwork

Covers are written into the existing on-disk image cache under
`ImageRole::Primary`, exactly like a sidecar or provider image. Consequence: the
existing `has_primary_art` denormalisation and the `set_artwork` writer apply
unchanged — and the B155 lesson applies, that `put` does not maintain
`has_primary_art`, so cover extraction must go through `set_artwork`.

## Scan flow

```
walk → extension classify
  ├─ media ext  → probe_one (ffprobe/libav) → MediaItem{probe}
  └─ book ext   → read_book_meta            → MediaItem{book, probe: default}
                        │
                        └─ cover bytes → set_artwork(Primary)
```

The classify step happens BEFORE the prober is reached (R4). SC-002 is asserted
by a test that counts prober invocations for a book path — a spy `Prober`, no
ffmpeg needed, so it satisfies V12 (domain logic testable with no ffmpeg).

## Extension sets

```
BOOK_EXTENSIONS   = epub, pdf, cbz, cbr, cbt, cb7, mobi, azw3
READABLE_BY_CLIENT= epub, pdf, cbz, cbr, cbt, cb7
```

`DEFAULT_EXTENSIONS` (`fs.rs:21`) gains the book set, so book files are walked at
all. The two lists are separate because "pharos indexes it" and "a client can
open it" are different facts, and conflating them is what would make a `.mobi`
render an open button that fails.
