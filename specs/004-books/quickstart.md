# Quickstart / validation: native book support

Proves the feature end to end against an unmodified client. Run in order — each
step's failure has a distinct meaning, noted so a red step points somewhere.

**Revised 2026-07-29**: step 3's assertions match R9 (empty/0, not absent/null),
step 4 gained the `Fields`-spelling check, and steps 5 and 8 are new.

## Prerequisites

- Nix devShell: everything below is `nix develop --command …`.
- A books root with one of each: `.epub`, `.cbz`, `.pdf`.
  Freely-licensed fixtures: any Standard Ebooks epub; a `.cbz` is a zip of
  numbered JPEGs (`zip test.cbz 001.jpg 002.jpg`); any PDF.

```toml
# config.toml
[[media.libraries]]
path = "/srv/Books"
name = "Books"
kind = "books"
```

## 1. Scan imports books without touching ffmpeg

```
nix develop --command cargo run -- scan --once
```

Expect: three items imported. **No ffmpeg/ffprobe invocation for any book path**
(SC-002).

- Zero items → the extension set never reached the walker (`DEFAULT_EXTENSIONS`).
- Items missing and a probe error in the log → the probe bypass (R4) is not in
  front of the prober; books are being handed to ffmpeg.

## 2. The library presents as books

```
curl -s -H "$AUTH" "$BASE/UserViews" | jq '.Items[]|{Name,CollectionType}'
```

Expect `{"Name":"Books","CollectionType":"books"}`.

Anything else (`mixed`) → `LibraryKind::parse` did not learn `books`, and
jellyfin-web will render a video grid.

## 3. The DTO satisfies the player gates

```
curl -s -H "$AUTH" "$BASE/Items?IncludeItemTypes=Book&Fields=CanDownload,Path" \
  | jq '.Items[]|{Name,Type,MediaType,Path,CanDownload,RunTimeTicks,MediaSources,MediaStreams,ImageTags}'
```

Expect for each: `Type:"Book"`, `MediaType:"Book"`, a real `Path`,
`CanDownload:true`, `RunTimeTicks:0`, `MediaSources:[]`, `MediaStreams:[]`
(SC-004).

- **`Path` absent → the feature will fail silently in the browser**; every
  reader's `canPlayItem` returns false with no error (R2). This is the single
  highest-value assertion here.
- `MediaType:"Video"` → `dto.rs:1588`'s `_ => "Video"` wildcard still wins. The
  compiler does not catch this (R10); it is the most likely single point of
  failure in the whole feature.
- A non-empty `MediaSources` → R9 breached; a client may try to transcode an epub.
- `ImageTags.Primary` present on a cover-less book → `dto.rs:1880` still
  advertises unconditionally; the grid will 404 on every render (B149 shape).

## 4. `Path` is genuinely `Fields`-gated, in every spelling

```
curl -s -H "$AUTH" "$BASE/Items?IncludeItemTypes=Book" | jq '.Items[0]|has("Path")'
curl -s -H "$AUTH" "$BASE/Items?IncludeItemTypes=Book&fields=path" | jq '.Items[0]|has("Path")'
```

Expect `false` then `true`. The second is V69: a client dialect that spells the
parameter in camelCase must not silently lose the whole feature.

## 5. A book offers nothing to play

```
ID=$(curl -s -H "$AUTH" "$BASE/Items?IncludeItemTypes=Book&Limit=1" | jq -r .Items[0].Id)
curl -s -X POST -H "$AUTH" "$BASE/Items/$ID/PlaybackInfo" | jq '{MediaSources,TranscodingUrl:.MediaSources[0].TranscodingUrl}'
```

Expect `MediaSources: []` and no transcoding URL (FR-010). A populated array here
means a book entered codec negotiation — the risk this requirement exists for.

## 6. Bytes come back, with a truthful length

```
curl -sI "$BASE/Items/$ID/Download?api_key=$TOKEN" | grep -iE 'content-length|content-type|accept-ranges'
curl -s -o /tmp/out.epub "$BASE/Items/$ID/Download?api_key=$TOKEN" && ls -l /tmp/out.epub
curl -s -H "Range: bytes=0-99" -o /dev/null -w '%{http_code} %{size_download}\n' \
  "$BASE/Items/$ID/Download?api_key=$TOKEN"
```

Expect: a HEAD `Content-Length` equal to the file size (**not 0** — B166/V113),
`Accept-Ranges: bytes`, a full body matching that size, and `206 100` for the
range request.

`Content-Length: 0` on the HEAD → the sized-body mistake B166 fixed on the image
path, repeated here.

Also verify query auth alone is enough — no `Authorization` header is sent above
and it must still succeed, because that is exactly how the client calls it.

## 7. Covers

```
curl -s -o /tmp/cover.jpg "$BASE/Items/$ID/Images/Primary" && file /tmp/cover.jpg
```

Expect a real image (SC-003). A 404 with `ImageTags.Primary` present is the B149
shape — advertising an image that does not exist.

Note what is expected to have **no** cover, by design: `.cbr` (no rar reader,
R7), a text-first `.pdf` (no rasteriser, R11), `.mobi`/`.azw3`. These must
advertise no tag rather than 404.

## 8. The scan can explain itself

```
curl -s "$BASE/metrics" | grep pharos_book_classify_total
```

Expect one series per `(format, verdict, reason)` seen, and:

```
# SC-003 — the cover rate, as a query rather than a manual count
sum by (verdict) (pharos_book_classify_total{verdict=~"cover_.*"})
# SC-005 — what each file was classified as
sum by (format) (pharos_book_classify_total)
```

`/metrics` is on the **main HTTP port**, not 9090. Absent series → the R12
instrumentation is missing, and SC-003/SC-005 are unverifiable.

## 9. Read it in the real client (the actual acceptance test)

```
nix develop --command just compat-playwright-full
```

Then by hand, which is what SC-001 means:

1. Open the Books library → covers render.
2. Open the epub → `bookPlayer` opens, **turn a page**, open the table of contents.
3. Open the `.cbz` → `comicsPlayer` opens, page forward.
4. Open the `.pdf` → `pdfPlayer` renders page 1.

If an item's card opens nothing at all and the network tab shows no `/Download`
request, it is gate 1 or 2 (`MediaType` / `Path`), not the bytes.

## 10. Progress

Read a few pages, navigate away, reopen.

```
curl -s -H "$AUTH" "$BASE/Items/$ID" | jq .UserData
```

Expect a non-zero `PlaybackPositionTicks`.

**Then look at the player UI.** `RunTimeTicks` is 0 (R8/R9), so no meaningful
progress bar can be computed. Confirm the client renders **no bar** rather than a
broken, full or NaN one — these are different outcomes and only one is
acceptable. This must be observed, not assumed, and it must not be "fixed" by
inventing a runtime.

## Signals to check after deploying

```
sum by (verdict) (pharos_book_classify_total)
sum by (provider) (pharos_metadata_field_source_total{field="production_year"})
```

The first is this feature's own decision counter (R12). The second is B169's
provenance counter, reused because book metadata flows through the same
resolver — no new metric for a question that already has one.
