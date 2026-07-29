# Phase 1 data model: native book support

**Revised 2026-07-29** after `/speckit-analyze`: the extension-set duplication is
gone, the DTO projection reflects R9, and the claim that the compiler enumerates
every decision site is corrected per R10.

## Changed enums

### `pharos_core::MediaKind` (+1 variant)

```
Movie | Episode | Audio | Book
```

- `as_str()` → `"book"` (the store discriminator).
- `from_wire()` accepts `"book"` case-insensitively (T88(d): the single
  canonical parser for the wire `Type` discriminator).

**What the compiler will and will not do.** Exhaustive `match` sites fail to
compile and must decide about books — that part holds. But **44 sites decide on
item kind without an exhaustive match** (34 `matches!`/`==`, 10 wildcard-arm
matches; R10), and the compiler flags none of them. Two of those sites are the
ones that decide whether the feature works at all:

| Site | Today | Needed for a book |
|---|---|---|
| `dto.rs:1588` `match item.kind { Audio => "Audio", _ => "Video" }` | a Book becomes `MediaType: "Video"` | `"Book"` — this is FR-002's gate |
| `dto.rs:1592` `is_video = !matches!(kind, Audio)` | a Book is treated as video | not video |
| `dto.rs:1880` `has_primary = !matches!(kind, Audio) \|\| has_primary_art` | a Book always advertises a Primary tag | only when a cover exists |
| `dto.rs:1887` / `1720` backdrop + thumb tags | a Book advertises both | neither |
| `filename.rs:206` `matches!(kind, Movie \| Episode)` | a Book gets no filename title | admitted, so FR-007's fallback works |

Adding the variant is still the right modelling choice — it makes the kind a
closed set rather than a string — but it is **not** a safety net, and the design
no longer claims it is.

### `pharos_core::LibraryKind` (+1 variant)

```
Movies | TvShows | Music | Books | Mixed
```

- `collection_type()` → `"books"` — a token the deployed jellyfin-web already
  recognises.
- `parse()` accepts `books`, `book`.

## New: `BookMeta`

Book-**specific** facts read from the file at scan time. Lives on the item the
way `MediaProbe` does for media, and is absent for a non-book item.

| Field | Type | Source | Notes |
|---|---|---|---|
| `format` | `BookFormat` | extension | Epub, Pdf, Comic, Unreadable |
| `page_count` | `Option<u32>` | comic entry count / pdf page count | epub has no stable page count — stays `None`, deliberately |
| `author` | `Option<String>` | epub `dc:creator`, ComicInfo `Writer` | |
| `publisher` | `Option<String>` | epub `dc:publisher` | |
| `series_name` | `Option<String>` | `calibre:series`, ComicInfo `Series` | |
| `series_index` | `Option<u32>` | `calibre:series_index`, ComicInfo `Number` | drives sort within a series; `None` sorts last, not as zero |
| `isbn` | `Option<String>` | epub `dc:identifier` | |

**Not here, deliberately**: title, release date and description. Those are
ordinary item fields on `MediaItem`, populated by the metadata resolver from the
same OPF/ComicInfo read (R6). FR-007 requires them; duplicating them into
`BookMeta` would create a second authority for a field every other item kind
already has, and the DTO would then have to decide which one wins.

### `BookFormat` is the single authority on readability

```rust
enum BookFormat { Epub, Pdf, Comic, Unreadable }
```

`Unreadable` is explicit and load-bearing: `.mobi`/`.azw3` are browsable but have
no client reader (spec Assumptions). Modelling that as a variant means the DTO
builder cannot forget it.

**One authority, not two.** The earlier draft also specified a
`READABLE_BY_CLIENT` extension list — which contradicted the very reason for the
variant, since "can a client read this?" would then have two answers that could
drift. There is one:

```rust
impl BookFormat {
    fn readable_by_client(self) -> bool { !matches!(self, BookFormat::Unreadable) }
}
```

`BOOK_EXTENSIONS` remains, because "does pharos walk this file?" is a genuinely
different question, asked before any `BookFormat` exists.

### Why not reuse `MediaProbe`?

`MediaProbe` is stream-shaped (codecs, pixel format, frame rate, tracks). Nesting
book facts inside it would put "author" beside "pix_fmt" and would make SC-004
(nothing to play on a book) unstateable. Separate struct, absent when not a book
— the same pattern `series: Option<SeriesInfo>` already uses for episodes.

## Changed: `MediaItem`

```
+ pub book: Option<BookMeta>,
```

`None` for every non-book item. `MediaItem.probe` stays as-is and is
`MediaProbe::default()` for a book — already documented as the "probe failure or
pre-ffprobe scan still yields a row" case, so no semantics change.

## Store

New nullable columns on `media_items`, one migration (`0052_book_metadata.sql`,
the next free number), no backfill — every existing row is `NULL`:

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

`MEDIA_COLUMNS` gains the seven columns — **in both files**, `sqlite.rs:16` and
`postgres.rs:31`, which are separate string constants. `MediaRow::into_domain`
assembles `BookMeta` when `book_format` is non-null.

## DTO projection

| Wire field | Value for a book | Where |
|---|---|---|
| `Type` | `"Book"` | |
| `MediaType` | `"Book"` | `dto.rs:1588` — currently `_ => "Video"` |
| `Path` | real path — **only when `Fields` requests it**, any spelling | new field |
| `CanDownload` | `true` | |
| `RunTimeTicks` | **`0`** — not null; the field is `u64` (`dto.rs:416`) | |
| `MediaSources` / `MediaStreams` | **empty arrays** — not absent (R9) | `dto.rs:419` |
| `SeriesName` / `IndexNumber` | from `book.series_name` / `series_index` | |
| `ProductionYear` / `PremiereDate` / `Overview` | from the metadata provider, as for any item | |
| `ImageTags.Primary` | present **iff** a cover was extracted | `dto.rs:1880` — currently unconditional for non-audio |
| `ImageTags.Backdrop` / `Thumb` | absent | `dto.rs:1887`, `1720` |
| `UserData.PlaybackPositionTicks` | reader position (R8) | |

`Path` reuses the existing `Fields`-gating helper family (`fields_requests`,
`items.rs:4874`) rather than a new mechanism.

**Empty, not absent** is the whole of R9: array fields are default-empty across
pharos because jellyfin-web iterates them without null guards (`dto.rs:420`), so
omitting them would trade a transcode risk for a client crash.

## Cover artwork

Covers are written into the existing on-disk image cache under
`ImageRole::Primary`, exactly like a sidecar or provider image. Consequence: the
existing `has_primary_art` denormalisation and the `set_artwork` writer apply
unchanged — and the B155 lesson applies, that `put` does not maintain
`has_primary_art`, so cover extraction must go through `set_artwork`.

Per format (R6, R11):

| Format | Cover source | Failure |
|---|---|---|
| epub | OPF `<meta name="cover">` → manifest href, else first spine image | `no_cover_entry` |
| cbz / cb7 | first image entry in name order | `no_cover_entry` |
| cbr | **none** — no rar reader (R7) | `rar_unsupported` |
| pdf | page one's embedded image when `DCTDecode` (already a JPEG) | `unsupported_image_encoding` |
| mobi / azw3 | none | `format_unreadable` |

Every failure reason is a bounded label on `pharos_book_classify_total` (R12), so
SC-003's rate is a query rather than a manual count.

## Scan flow

```
walk → extension classify ─── record verdict on pharos_book_classify_total
  ├─ media ext  → probe_one (ffprobe/libav) → MediaItem{probe}
  └─ book ext   → read_book_meta            → MediaItem{book, probe: default}
                        │
                        └─ cover bytes → set_artwork(Primary)
```

The classify step happens BEFORE the prober is reached (R4). SC-002 is asserted
by a test that counts prober invocations for a book path — a spy `Prober`, no
ffmpeg needed, so it satisfies V12 (domain logic testable with no ffmpeg). The
same branch emits the R12 counter, which is what makes SC-003 and SC-005
answerable from the running server.

The filesystem watcher needs no separate change: `watcher.rs:312` →
`update_path` → `probe_put_one` → `probe_one`, and its filter uses
`extensions_snapshot()` (verified, R4).

## Extension sets

```
BOOK_EXTENSIONS = epub, pdf, cbz, cbr, cbt, cb7, mobi, azw3
```

Added to `DEFAULT_EXTENSIONS` (`fs.rs:20`) so book files are walked at all.
Readability is **not** a second list — it is `BookFormat::readable_by_client()`.
