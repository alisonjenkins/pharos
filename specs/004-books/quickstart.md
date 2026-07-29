# Quickstart / validation: native book support

Proves the feature end to end against an unmodified client. Run in order — each
step's failure has a distinct meaning, noted so a red step points somewhere.

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
  | jq '.Items[]|{Name,Type,MediaType,Path,CanDownload,RunTimeTicks,MediaSources}'
```

Expect for each: `Type:"Book"`, `MediaType:"Book"`, a real `Path`,
`CanDownload:true`, `RunTimeTicks:null`, `MediaSources:null` (SC-004).

- `Path` absent → **the feature will fail silently in the browser**; every
  reader's `canPlayItem` returns false with no error (R2). This is the single
  highest-value assertion here.
- `MediaSources` present → R9 breached; a client may try to transcode an epub.

Then confirm it is genuinely `Fields`-gated:

```
curl -s -H "$AUTH" "$BASE/Items?IncludeItemTypes=Book" | jq '.Items[0]|has("Path")'
```

Expect `false`.

## 4. Bytes come back, with a truthful length

```
ID=$(curl -s -H "$AUTH" "$BASE/Items?IncludeItemTypes=Book&Limit=1" | jq -r .Items[0].Id)
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

## 5. Covers

```
curl -s -o /tmp/cover.jpg "$BASE/Items/$ID/Images/Primary" && file /tmp/cover.jpg
```

Expect a real image (SC-003). A 404 with `ImageTags.Primary` present is the B149
shape — advertising an image that does not exist.

## 6. Read it in the real client (the actual acceptance test)

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

## 7. Progress

Read a few pages, navigate away, reopen.

```
curl -s -H "$AUTH" "$BASE/Items/$ID" | jq .UserData
```

Expect a non-zero `PlaybackPositionTicks`. Note `RunTimeTicks` is null by design
(R8), so no progress BAR renders — that is expected, not a bug, and must not be
"fixed" by inventing a runtime.

## Signals to check after deploying

```
sum by (provider) (pharos_metadata_field_source_total{field="production_year"})
```

Book metadata flows through the same resolver, so its provider shows up here
(B169's counter, reused). Add nothing new until this is insufficient.
