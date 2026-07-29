# Phase 0 research: native book support

Every finding below was established by reading the DEPLOYED jellyfin-web bundle
and the pharos tree, not from memory of how Jellyfin works. The bundle is the
acceptance test (Constitution I), so it is the primary source.

Bundle read: `jellyfin-web` 10.11.x as shipped in the running
`pharos-jellyfin-web` image.

**Revised 2026-07-29** after `/speckit-analyze`: R9 rewritten, R10–R12 added.
R10 corrects the assumption the rest of the plan had been resting on.

---

## R1 — Where does the reader live?

**Decision**: pharos implements NO reader. It is a metadata + bytes problem.

**Rationale**: the bundle already contains three registered players:

| Player | Chunk | Renders |
|---|---|---|
| `bookPlayer` | `bookPlayer-plugin.1e8df074…` | epub, via epub.js |
| `pdfPlayer` | `pdfPlayer-plugin.5717ee14…` | pdf, via pdf.js |
| `comicsPlayer` | `comicsPlayer-plugin.6bae3e98…` | cbr/cbt/cbz/cb7, via libarchive.js |

This is the single most important finding: it collapses the feature from
"implement a reading experience" to "satisfy three gates".

**Alternatives considered**: a Dioxus reader in `pharos-ui`. Rejected for now —
it would duplicate work the acceptance client already does, and the UI is not
the compat surface the constitution prioritises.

## R2 — What exactly do the players require?

**Decision**: three gates, all of which pharos must satisfy exactly.

1. **`canPlayMediaType(mediaType)`** — all three are literally
   `return "book" === (e || "").toLowerCase()`. So `MediaType` must be `Book`.
2. **`canPlayItem(item)`** — reads **`item.Path`**:
   - `bookPlayer`: `item.Path?.endsWith("epub")` — **case-sensitive**
   - `pdfPlayer`: `!!item.Path && item.Path.toLowerCase().endsWith("pdf")`
   - `comicsPlayer`: `item.Path && [".cbr",".cbt",".cbz",".cb7"].some(t => item.Path.endsWith(t))`
3. **Bytes** — all three call `getItemDownloadUrl(item.Id)`, which builds
   `Items/{id}/Download?api_key=<token>`.

**Consequence**: pharos currently emits **no `Path` field at all** (no such field
in `BaseItemDto`, `dto.rs:407`) and has **no `/Items/{id}/Download` route** (only
`/Items/{id}/RemoteImages/Download`). Both are hard blockers. Neither produces
an error — `canPlayItem` returning false means the client declines silently,
which is the exact failure shape recorded in the jellyfin-web-unguarded-endpoint
class of bugs.

**Alternatives considered**: reporting a synthetic `Path` (e.g. a URL) to satisfy
the extension test. Rejected — the field means a filesystem path, other client
surfaces (the metadata editor) display it, and lying here would surface as a
different bug later.

## R3 — Does exposing `Path` violate V9?

**Decision**: no, and it is `Fields`-gated regardless.

**Rationale**: V9 is "media paths never reach an *unauthenticated* client".
`/Items/{id}/Download` and item fetches are authenticated. Separately, the client
asks for the field explicitly — the bundle issues `Fields:"CanDownload,Path"` —
so pharos emits `Path` only when `Fields` contains `Path`, matching the existing
`Fields`-gating machinery (`items.rs:4874`, `fields_requests`, the same pattern
that keeps `People` off the default payload). Least exposure, no new default
surface.

**Amended for V69**: the gate must accept every spelling a client dialect may
send (`Fields`/`fields`, `Path`/`path`). Ignoring a camelCase spelling of a query
parameter is a recurring bug class in this project, and here it would silently
disable the whole feature for whichever client spells it differently.

**Note for the Constitution Check**: this is adjacent to V9, so the invariant
gets an explicit clause rather than being left to interpretation.

## R4 — How do books get through a probe-centric scanner?

**Decision**: books bypass the prober entirely; a `BookMeta` reader replaces it.

**Rationale**: `probe_one` (`fs.rs:988`) calls `self.prober.probe(&path)` and
returns `None` on failure (V6 — a probe miss writes nothing, `fs.rs:872`). An
epub is not a media container, so ffprobe/libav fails and the item is never
imported. `DEFAULT_EXTENSIONS` (`fs.rs:20`) is also video/audio only, so book
files are not even walked today.

The scan pipeline must branch on extension BEFORE the probe: a book path goes to
a book metadata reader and yields a `MediaItem` with `MediaProbe::default()`.
`MediaItem.probe` is already documented as all-optional precisely so a row can
exist without probe data, so no struct change is forced.

**Verified — the filesystem watcher needs no separate work**: `watcher.rs:312`
calls `FsScanner::update_path`, which routes to `probe_put_one` → `probe_one`,
and the watch filter uses `extensions_snapshot()`. So extending
`DEFAULT_EXTENSIONS` and branching inside `probe_one` covers runtime-added books
for free. This was checked rather than assumed.

**Alternatives considered**: a `Prober` impl that recognises books. Rejected —
`Prober` returns `ProbeInfo { kind, probe }` shaped around streams, and it would
put epub XML parsing behind an interface named for ffmpeg. Worse, SC-002 (no
ffmpeg invocation for a book) would be untestable through it.

## R5 — Where does `Book` fit in the existing type system?

**Decision**: a new `MediaKind::Book`, and a new `LibraryKind::Books`.

**Rationale**: `MediaKind` is `{Movie, Episode, Audio}` (`lib.rs:1929`) with
`as_str` and `from_wire`; `LibraryKind` is `{Movies, TvShows, Music, Mixed}`
(`lib.rs:1402`) with `collection_type()` → the wire token. `books` is already a
collection-type token jellyfin-web understands (present in the bundle).

**Blast radius — and the correction**: the first version of this research said
adding a variant "makes every site that must decide about books fail to
compile". That is **only true of exhaustive `match`**. See R10 — it is not true
of the sites that matter most. The variant is still the right modelling choice;
it is just not a safety net.

## R6 — Where does book metadata come from?

**Decision**: from the file, via a new provider in the EXISTING resolver.

**Rationale**: `MetadataResolver` already merges priority-ordered providers
(nfo 100 > sidecar 50 > embedded 30 > filename 10) and books need the same
shape: a `.opf`/ComicInfo reader is just another provider at embedded priority
(30). It inherits the V6 isolation (a malformed epub is logged and skipped, the
item still imports) and the B169 provenance counter
(`pharos_metadata_field_source_total`) for free.

**`quick-xml` is already a `pharos-scanner` dependency** (`Cargo.toml:29`), so
`container.xml`, the OPF and `ComicInfo.xml` need no new parser.

Sources per format:

| Format | Metadata | Cover |
|---|---|---|
| epub | `META-INF/container.xml` → OPF `<metadata>` (Dublin Core: title, creator, publisher, **date**, **description**; `calibre:series`) | OPF `<meta name="cover">` → manifest href, else first image in spine |
| cbz/cb7 | `ComicInfo.xml` if present (Series, Number, Writer, Summary) | first image entry in name order |
| cbr/cbt | as above | as above (but see R7) |
| pdf | document info dictionary | page one's embedded image (R11) |

**FR-007's release date and description**: OPF `dc:date` and `dc:description` map
onto the item's ordinary release-date and overview fields, NOT onto `BookMeta`.
Books do not get a parallel metadata path — that is the whole point of adding a
provider to the existing resolver. `BookMeta` holds only what is
book-*specific* (format, page count, author, publisher, series, ISBN).

**Title fallback**: `metadata/filename.rs:206` gates the filename provider on
`matches!(kind, Movie | Episode)`, so a book currently gets no filename-derived
title. FR-007 requires no book is ever listed untitled, so books are admitted to
that provider. One of R10's audit sites.

**Alternatives considered**: an online provider (Google Books / Comic Vine) as
step one. Rejected — the local file already carries most of it, and a provider
adds a rate-limited network dependency plus a match-quality problem (the exact
class that produced B150/B152/B159/B160 for album art). Local first; a provider
slots into the same resolver afterwards.

## R7 — Reading a comic archive: what does the SERVER need to unpack?

**Decision**: only enough to find the cover. The client unpacks for reading.

**Rationale**: `comicsPlayer` downloads the whole archive and unpacks it in the
browser with libarchive.js. So the server never enumerates pages for delivery.
It reads the archive once at scan time for the cover image and the page count.

- `cbz` is a zip; `cb7` is 7z. Neither crate is in `Cargo.lock` today, so `zip`
  and `sevenz-rust` are genuinely new deps (both pure Rust).
- `cbr` is RAR: **unrar is not in the devShell and rar is patent-encumbered**.
  Recorded as a gap: `.cbr` files will be listed and downloadable and WILL read
  in the client (libarchive.js handles rar), but pharos cannot extract their
  cover without a rar reader. Cover-less-but-readable is the honest outcome; the
  alternative (claiming cover support and 404ing) is the B149 failure shape.

**Resolved by the R11 precedent** (this was open in the first draft): **no rar
reader.** `unrar` wraps a C library, which breaks single-binary deploy — the
identical objection that rules out a PDF rasteriser, so it is settled by the same
principle rather than deferred. `.cbr` is permanently cover-less-but-readable,
reported as `rar_unsupported` on the R12 counter so the count is visible rather
than mysterious.

Revisit only if a pure-Rust rar decoder becomes available; the trigger is a new
crate existing, not a `.cbr` appearing in the library.

## R8 — Read progress

**Decision**: reuse the existing per-user `UserData`; store the reader's opaque
position.

**Rationale**: pharos already persists `PlaybackPositionTicks` / `Played` per
user. `bookPlayer` reports progress through the normal playback-reporting path,
so no new table is required. Ticks are a nonsense UNIT for a book (there is no
time axis) but they are an opaque integer on the wire, and inventing a parallel
mechanism would break the stock client.

**Consequence for FR-008**: `RunTimeTicks` is **0** (see R9 — null is not
available). Progress bars computed as position/runtime will therefore not render
a meaningful bar. Accepted — but "renders no bar" and "renders a broken bar"
are different outcomes and only one is acceptable, so this is **observed during
acceptance, not assumed**. The reason is recorded so it is not later "fixed" by
inventing a fake runtime.

## R9 — What must NOT appear on a book DTO

**Decision**: `MediaSources` and `MediaStreams` are **empty**, `RunTimeTicks`
is **0**, and no `MediaSources` entry exists for a client to act on. Empty —
not absent, not null.

**Rationale**: the goal is that no client can request a stream or transcode of
an epub. Emptiness achieves that completely; the JSON spelling of nothing is
immaterial to the goal and very material to something else.

The earlier version of this decision said "no `MediaSources` array, non-null
`RunTimeTicks`". Both are **unavailable** and one is actively harmful:

- `BaseItemDto.run_time_ticks` is `u64` (`dto.rs:416`) — not `Option`, no
  `skip_serializing_if`. `null` cannot be produced without changing the field
  for every item kind.
- `media_sources` is `Vec<MediaSourceLiteDto>` (`dto.rs:419`), so it serialises
  as `[]`. Making it absent means adding `skip_serializing_if` — and the comment
  at `dto.rs:420` exists precisely to forbid that: array-typed fields are
  default-empty because *"jellyfin-web iterates over [them] without null guards
  (T30). Default-empty so for-of / spread / .map don't throw Symbol.iterator
  TypeErrors during view init."* Omitting them reopens a client crash class this
  project already fixed.

**Consequence**: FR-008 and SC-004 were restated in the spec. The assertion is
"nothing to act on", verified at `/Items`, `/Items/{id}` and `PlaybackInfo`.

**`PlaybackInfo` is in scope, not merely "unchanged"**: the route exists at
`items.rs:105-106` (both GET and POST) with the handler at `items.rs:2081`. A
book id reaching it must leave without entering codec negotiation. This is now
FR-010, because leaving it in an HTTP contract document meant it had no task.

## R10 — Does the type system actually catch the sites that decide about books?

**Decision**: **No.** Audit them by hand, before adding the variant.

This is a correction. The plan's Constitution-Check justification for principle V
claimed that adding a `MediaKind` variant "makes every site that must now decide
about books fail to compile — that is the point of adding a variant rather than a
boolean". The compiler only enforces exhaustiveness for `match` **without a
wildcard arm**. Measured over the tree, excluding test modules:

| Shape | Count | Compiler flags it? |
|---|---|---|
| `matches!(kind, …)` / `kind == MediaKind::…` | **34** | no |
| `match kind { … _ => … }` (wildcard arm) | **10** | no |

Spread across 20 files — `items.rs` (11), `dto.rs` (6 + the two wildcard
matches), `image_cache.rs` (3), `fs.rs`, `filename.rs`, `images.rs`,
`dlna_xml.rs` (3 wildcards), `tmdb.rs`/`tvdb.rs`, `trickplay_backfill.rs`,
`waveform.rs`, `hls.rs`, and the Dioxus UI.

The two that matter most are adjacent:

```rust
// dto.rs:1588 — decides FR-002's gate. A Book silently becomes "Video".
let media_type = match item.kind {
    pharos_core::MediaKind::Audio => "Audio",
    _ => "Video",
};
let is_video = !matches!(item.kind, pharos_core::MediaKind::Audio);
```

```rust
// dto.rs:1880 — a Book gets a Primary image tag unconditionally, so a
// cover-less book 404s on every grid render. The B149 shape.
let has_primary = !matches!(item.kind, pharos_core::MediaKind::Audio) || item.has_primary_art;
```

**Rationale for auditing first**: after the variant is added the tree compiles
clean and every one of these sites becomes invisible. The audit is only cheap
while `Book` does not exist yet. Its output is a verdict per site — "books
behave like video here", "books are excluded here", "this site must now branch" —
recorded so the reviewer of the variant commit can check the decisions rather
than re-derive them.

**Alternatives considered**: a lint or a newtype wrapper forcing exhaustive
handling. Rejected as disproportionate for one variant addition — but worth
revisiting if a fifth `MediaKind` ever lands, since this cost recurs every time.

## R11 — PDF covers without a rasteriser

**Decision**: extract page one's **embedded image** when it is pass-through
encodable (DCTDecode, i.e. JPEG). No rasterisation. A text-first PDF gets no
cover and advertises none.

**Rationale**: `lopdf` parses the page tree, resources and XObject streams; it
does not render. Rendering needs pdfium, mupdf or poppler — all C libraries,
which breaks "single-binary deploy, no runtime deps beyond ffmpeg", and ffmpeg
cannot read PDF.

Two further facts narrow it, both checked:

- A scanned book or comic-as-PDF is one full-page image per page, and that image
  is normally `DCTDecode` — the stream bytes **are** a JPEG, so they go straight
  into the existing image cache with no decoding step at all.
- **There is no pure-Rust image decoder anywhere in the tree** — no `image`
  crate in `Cargo.lock`; all image work goes through libav. So a
  `FlateDecode` raw-sample bitmap cannot be turned into a JPEG without either a
  new decode dependency or a libav round-trip through a path built for media
  files. Out of scope: those PDFs get no cover.

**Consequence**: this is narrower than FR-006's "page one's embedded image"
implies, and the narrowing is recorded rather than discovered later. If
text-first PDF covers turn out to matter, the decision to revisit is "add an
image encoder", not "add a rasteriser".

**Alternatives considered**: shelling out to `pdftoppm`; a WASM pdf.js in-process.
Both rejected as runtime dependencies far larger than the feature.

## R12 — Proving the classification by query

**Decision**: one counter,
`pharos_book_classify_total{format,verdict,reason}`, shipped in its own commit
**before** the branch it measures.

**Rationale**: Constitution III requires that any branch choosing between
behaviours record its inputs, its verdict and the reason, carrying the offending
value rather than a bare class. The scan's media-vs-book branch is exactly such
a branch, and the earlier plan argued ODD was thin here because a book that will
not open fails visibly. That argument covers the *symptom* and not the decision:
"this file was classified `Unreadable` because its extension is `.azw3`" and
"this file yielded no cover because page one was `FlateDecode`" are not visible
from the outside at all.

It also closes two otherwise-unmeasurable success criteria:

- **SC-003** (≥95% of covers present) becomes
  `sum by (verdict) (pharos_book_classify_total{verdict=~"cover_.*"})`.
- **SC-005** (which format each file was classified as) is the `format` label.

Labels are a dashboard contract: bounded cardinality, stable strings from a
`label()` method, asserted distinct in a test. `reason` carries a bounded
enumerated cause (`no_cover_entry`, `unsupported_image_encoding`,
`rar_unsupported`, `malformed_container`), never a free-form message — the
offending *value* goes in the log line beside it.

**Alternatives considered**: logs only. Rejected — SC-003 is a rate, and rates
come from counters. Reusing `pharos_metadata_field_source_total` was considered
and rejected: that counter answers "which provider supplied this field", a
different question, and overloading it would break its existing meaning.

---

## Resolved / open summary

| Id | Status |
|---|---|
| R1 client-side readers | resolved — no server reader |
| R2 player gates | resolved — MediaType Book + Fields-gated Path + /Download |
| R3 V9 tension | resolved — authenticated + Fields-gated (all spellings); invariant clause added |
| R4 probe bypass | resolved — branch before the prober; watcher covered for free |
| R5 type system | resolved — new MediaKind + LibraryKind variants (but see R10) |
| R6 metadata source | resolved — new provider at embedded priority; quick-xml reused |
| R7 rar cover extraction | resolved — no rar reader (C dep; same principle as R11); `.cbr` is cover-less by design |
| R8 progress | resolved — existing UserData, runtime 0, bar absence observed not assumed |
| R9 forbidden fields | resolved — **empty, not absent**; PlaybackInfo in scope |
| R10 non-exhaustive sites | resolved — 44 sites, audited before the variant lands |
| R11 PDF covers | resolved — pass-through embedded JPEG only, narrower than FR-006 implies |
| R12 classification signal | resolved — one counter, ships before the branch |
