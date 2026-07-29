# Phase 0 research: native book support

Every finding below was established by reading the DEPLOYED jellyfin-web bundle
and the pharos tree, not from memory of how Jellyfin works. The bundle is the
acceptance test (Constitution I), so it is the primary source.

Bundle read: `jellyfin-web` 10.11.x as shipped in the running
`pharos-jellyfin-web` image.

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

**Consequence**: pharos currently emits **no `Path` field at all** (there is no
such field in `BaseItemDto`) and has **no `/Items/{id}/Download` route** (only
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
`Fields`-gating machinery in `items.rs` (the same pattern that keeps `People` off
the default payload). Least exposure, and no new default surface.

**Note for the Constitution Check**: this is adjacent to V9, so the invariant
gets an explicit clause rather than being left to interpretation.

## R4 — How do books get through a probe-centric scanner?

**Decision**: books bypass the prober entirely; a `BookMeta` reader replaces it.

**Rationale**: `probe_one` calls `self.prober.probe(&path)` and returns `None`
on failure (V6 — a probe miss writes nothing). An epub is not a media container,
so ffprobe/libav fails and the item is never imported. `DEFAULT_EXTENSIONS`
(`fs.rs:21`) is also video/audio only, so book files are not even walked today.

The scan pipeline must branch on extension BEFORE the probe: a book path goes to
a book metadata reader and yields a `MediaItem` with `MediaProbe::default()`.
`MediaItem.probe` is already documented as all-optional precisely so a row can
exist without probe data, so no struct change is forced.

**Alternatives considered**: a `Prober` impl that recognises books. Rejected —
`Prober` returns `ProbeInfo { kind, probe }` shaped around streams, and it would
put epub XML parsing behind an interface named for ffmpeg. Worse, SC-002 (no
ffmpeg invocation for a book) would be untestable through it.

## R5 — Where does `Book` fit in the existing type system?

**Decision**: a new `MediaKind::Book`, and a new `LibraryKind::Books`.

**Rationale**: `MediaKind` is `{Movie, Episode, Audio}` (`lib.rs:1929`) with
`as_str` and `from_wire`; `LibraryKind` is `{Movies, TvShows, Music, Mixed}`
(`lib.rs:1402`) with `collection_type()` → the wire token. Both are exhaustively
matched, so adding a variant makes every site that must decide about books fail
to compile — which is the property worth having. `books` is already a
collection-type token jellyfin-web understands (present in the bundle).

**Blast radius warning**: `MediaKind` is matched in the store, DTO builders, the
scanner and query layers. This is the single largest mechanical cost of the
feature and is why it is its own task with its own commit.

## R6 — Where does book metadata come from?

**Decision**: from the file, via a new provider in the EXISTING resolver.

**Rationale**: `MetadataResolver` already merges priority-ordered providers
(nfo 100 > sidecar 50 > embedded 30 > filename 10) and books need the same
shape: a `.opf`/ComicInfo reader is just another provider. It inherits the V6
isolation (a malformed epub is logged and skipped, the item still imports) and
the B169 provenance counter (`pharos_metadata_field_source_total`) for free.

Sources per format:

| Format | Metadata | Cover |
|---|---|---|
| epub | `META-INF/container.xml` → OPF `<metadata>` (Dublin Core: title, creator, publisher, date, description; `calibre:series`) | OPF `<meta name="cover">` → manifest href, else first image in spine |
| cbz/cb7 | `ComicInfo.xml` if present (Series, Number, Writer, Summary) | first image entry in name order |
| cbr/cbt | as above | as above |
| pdf | document info dictionary | rendered page 1 |

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

- `cbz` is a zip — read with the crate already in the tree if one is present,
  else `zip`.
- `cbr` is RAR: **unrar is not in the devShell and rar is patent-encumbered**.
  Recorded as a gap: `.cbr` files will be listed and downloadable and WILL read
  in the client (libarchive.js handles rar), but pharos cannot extract their
  cover without a rar reader. Cover-less-but-readable is the honest outcome; the
  alternative (claiming cover support and 404ing) is the B149 failure shape.

**NEEDS CLARIFICATION — not gating**: whether to add a pure-Rust rar reader
(`unrar` crate wraps a C lib; `sevenz-rust`/`zip` cover cb7/cbz). Decide when
`.cbr` actually appears in the library; it changes nothing else in the design.

## R8 — Read progress

**Decision**: reuse the existing per-user `UserData`; store the reader's opaque
position.

**Rationale**: pharos already persists `PlaybackPositionTicks` / `Played` per
user. `bookPlayer` reports progress through the normal playback-reporting path,
so no new table is required. Ticks are a nonsense UNIT for a book (there is no
time axis) but they are an opaque integer on the wire, and inventing a parallel
mechanism would break the stock client.

**Consequence for FR-008**: `RunTimeTicks` must stay null even though
`PlaybackPositionTicks` is used. Progress bars computed as position/runtime will
therefore not render — accepted, and the reason is recorded so it is not
"fixed" later by inventing a fake runtime.

## R9 — What must NOT appear on a book DTO

**Decision**: no `MediaSources`, no `MediaStreams`, no `RunTimeTicks`, and the
item must never reach `PlaybackInfo`'s negotiation path.

**Rationale**: those fields drive codec negotiation. A book with a
`MediaSources` array invites a client to request `/Videos/{id}/stream` and get a
transcode attempt on an epub. SC-004 asserts their absence.

---

## Resolved / open summary

| Id | Status |
|---|---|
| R1 client-side readers | resolved — no server reader |
| R2 player gates | resolved — MediaType Book + Fields-gated Path + /Download |
| R3 V9 tension | resolved — authenticated + Fields-gated; invariant clause added |
| R4 probe bypass | resolved — branch before the prober |
| R5 type system | resolved — new MediaKind + LibraryKind variants |
| R6 metadata source | resolved — new provider in the existing resolver |
| R7 rar cover extraction | **open, non-gating** — decide when a `.cbr` appears |
| R8 progress | resolved — existing UserData, runtime stays null |
| R9 forbidden fields | resolved — asserted by SC-004 |
