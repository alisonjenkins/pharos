# `MediaKind` decision-site audit (T004 / R10)

**Run**: 2026-07-29, on `main`+`004-books` at `a7a1ac2`, **before** `MediaKind::Book`
exists.

## Why this document exists

plan.md originally justified adding a `MediaKind` variant by claiming the
compiler would enumerate every site that must now decide about books. It does
not. `rustc` enforces exhaustiveness only for a `match` with no wildcard arm.
Every `matches!`, every `==`/`!=`, and every `match` with a `_ =>` arm silently
routes `Book` into an existing branch.

This list is only obtainable **now**. After the variant lands the tree compiles
clean and these sites become invisible — which is exactly when they stop being
auditable.

## Method

```
matches!(… MediaKind …)  |  kind ==/!= MediaKind  →  34 sites, ALL compiler-invisible
match … kind { … }       →  20 sites, exhaustiveness decided by rustc in T005
```

Excluded as different enums, not `MediaKind`: `StreamKind`
(`worker/ffi.rs:235`), `KindFilter` (`items.rs:4640`), and `pharos-ui`'s own
`ItemKind` (`player.rs:159,293`, `search.rs:29`).

**On the `match` sites**: several were initially miscounted as
wildcard-bearing because an inner `match (season, episode)` supplied a `_` arm
attributed to the outer block (`tmdb.rs:307`, `tvdb.rs:526`, `dto.rs:1583`,
`dlna_xml.rs:106`). Rather than guess, the `match` sites are left to the
compiler: T005 adds the variant, and every error it reports is an exhaustive
site that must decide. **The predicate sites below are the ones it will never
report**, and they are this document's real subject.

## Verdicts

`EXCLUDE` = a book must not be in this set; the existing predicate already
achieves it, no change needed. `CHANGE` = the existing predicate gives a book
the wrong answer and must be edited. `SAFE` = book falls into a branch that is
harmless for it.

### CHANGE — 7 sites, all in the DTO or the metadata path

| Site | Today | Book must be | Task |
|---|---|---|---|
| `dto.rs:1588` `match item.kind { Audio => "Audio", _ => "Video" }` | a Book's `MediaType` becomes **`"Video"`** | `"Book"` — **this is FR-002's gate; get this wrong and no reader opens anything** | T038 |
| `items.rs:6119` `media_type_of = \|kind\| match kind { Audio => "audio", _ => "video" }` | same bug, second site, on the resume/next-up path | `"book"` | T038 |
| `dto.rs:1592` `is_video = !matches!(kind, Audio)` | a Book is video; feeds `container_for` and `build_media_streams` | not video — no container claim, no streams | T038/T040 |
| `dto.rs:1880` `has_primary = !matches!(kind, Audio) \|\| has_primary_art` | a Book **always** advertises a `Primary` tag → cover-less books 404 on every grid render (B149 shape) | `has_primary_art` only | T039 |
| `dto.rs:1887` backdrop tag, `dto.rs:1720` `backdrop_image_tags` | a Book advertises backdrops it has none of | absent | T039 |
| `dto.rs:1701` chapter `image_tag` | non-audio gets a per-chapter frame tag (B88); a Book has no frames | `None` | T039 |
| `filename.rs:206` `matches!(kind, Movie \| Episode)` | a Book gets **no filename-derived title** | admitted — FR-007 requires no book is listed untitled | T067 |

### CHANGE — 1 more, found during the audit and not in the task list

| Site | Today | Book must be | Task |
|---|---|---|---|
| `fs.rs:1002` `match info.kind { Audio => probe.title.or(stem), _ => stem() }` | wildcard, so a Book already gets `stem()` — **correct by luck** | keep `stem()`, but the arm should name `Book` explicitly so the next variant does not inherit the accident | T036 |

### EXCLUDE — the predicate already keeps books out, no change needed

Verified individually; each is an audio-only or video-only feature that a book
has no business in.

| Sites | Feature |
|---|---|
| `dto.rs:1812` (`Movie` only) | media segments / skip-intro |
| `trickplay_backfill.rs:300`, `image_cache.rs:994` (`Movie\|Episode`) | trickplay tiles, backdrop extraction |
| `segment_backfill.rs:118`, `items.rs:1536,1690,1782,5869`, `sidecar.rs:103`, `tvdb.rs:504` (`Episode`) | episode/series-only paths |
| `items.rs:618,767,915,3761,3917,5294`, `image_cache.rs:1081,1092` (`Audio`) | audio-library grouping, album art |
| `waveform.rs:81` (`!Audio` → 404) | waveform is audio-only; a Book is rejected |
| `images.rs:578` (`Audio && Backdrop\|Thumb` → 404) | a Book is not audio, so it falls through to the Primary path — which is what we want, since that is where the cover lives |
| `images.rs:684` (`Audio && !has_primary_art`) | audio cover-art fallback |
| `items.rs:2128` (`Movie\|Episode`) | `is_video` for playback capability |
| `filename.rs:216` (`Movie`) | `(year)`-in-filename parsing; books rarely carry it and a wrong year is worse than none |
| `fs.rs:1022,1027` (`Movie`→`Episode` promotion, series info) | a Book is never promoted to an episode |
| `items.rs:4659` (`Episode` scoping) | query scoping |

### SAFE — reachable only by a path a book cannot take

| Site | Why |
|---|---|
| `hls.rs:722` `is_audio` | a book has empty `MediaSources`, so no client constructs an HLS URL for it. Left alone deliberately: adding a book branch here would imply books have a streaming path |
| `items.rs:2716` (`!Audio && h264 rendition`) | inside the HLS master-selection block, same reasoning |

### Needs a decision the compiler will force in T005

These are `match` blocks on `MediaKind`. If exhaustive, T005 will not compile
until each decides; if wildcard, the verdict below applies. Recorded so the
answer is chosen rather than defaulted.

| Site | Intended verdict for `Book` |
|---|---|
| `dto.rs:1583` `Type` | `"Book"` |
| `item_ops.rs:125` content type | book MIME per the `/Download` map |
| `items.rs:201` | inspect; likely audio/video split → exclude |
| `nfo.rs:127`, `metadata_backfill.rs:379,425,463` | **skip books** — no online provider for books (spec, Out of scope) |
| `tmdb.rs:229,293,307`, `tvdb.rs:513,526` | **`None` / no request.** `tmdb.rs:229` is the dangerous one: `_ => tv/{id}/images` would query TMDB's **TV** endpoint for a book |
| `dlna_xml.rs:106,110,114` | **exclude books from DLNA entirely.** `110`/`114` would otherwise advertise a book as `video/webm` on a `/Videos/{id}/stream` URL |
| `image_cache.rs:117,1029` | book covers use the existing Primary path; no ffmpeg args |
| `fs.rs:1002` | `stem()`, named explicitly (see CHANGE above) |

## Summary

| | Count |
|---|---|
| Predicate sites the compiler will **never** report | **34** |
| …of which require a code change | **8** |
| …correct as-is (exclude or safe) | **26** |
| `match` sites, verdict pre-recorded, exhaustiveness settled by rustc | 20 |

The single highest-consequence finding is that **two** separate sites decide
`MediaType` from a wildcard arm — `dto.rs:1588` and `items.rs:6119` — and both
would silently label a book `Video`. FR-002 is the gate every one of the three
client readers checks first, so this alone would have made the feature fail with
no error message anywhere.

Second is `dto.rs:1880`: a cover-less book would advertise a poster and 404 on
every grid render, which is the exact shape of B149.

Neither would have produced a compile error.
