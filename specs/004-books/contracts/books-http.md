# HTTP contract: native book support

Only the deltas. Everything else on these endpoints is unchanged.

Authority for every requirement below is the deployed jellyfin-web bundle
(see [research.md](../research.md) R2), not the Jellyfin OpenAPI document — where
they disagree, the shipped client wins, because it is the acceptance test.

---

## NEW `GET /Items/{itemId}/Download`

Serves the item's file bytes. This is the URL all three readers construct.

**Auth**: `?api_key=<token>` **or** the `Authorization: MediaBrowser …` header.
The client builds it with `api_key` in the query and **no header**
(`getItemDownloadUrl` → `Items/{id}/Download?api_key=…`), so query auth is not
optional. The existing extractor already accepts `api_key`
(`auth_extractor.rs:155`).

**Request**

| Header | Handling |
|---|---|
| `Range: bytes=…` | honoured; `206` with `Content-Range` |
| absent | `200` with full body |

**Response**

| | |
|---|---|
| `200` / `206` | file bytes |
| `Content-Length` | real byte length — on a `HEAD` too (V113/B166: a sized body, since actix derives the header from `BodySize` and discards a hand-set one) |
| `Content-Type` | `application/epub+zip`, `application/pdf`, `application/vnd.comicbook+zip`, `application/vnd.comicbook-rar`, else `application/octet-stream` |
| `Content-Disposition` | `attachment; filename="<basename>"` |
| `Accept-Ranges` | `bytes` |
| `404` | unknown id, or the file is gone from disk |
| `401` | no/!valid token |

**Path traversal**: the id resolves to a stored path; the request never carries a
path. V9 — traversal blocked at the boundary because there is nothing to traverse.

**Not restricted to books.** Real Jellyfin's `/Download` serves any item, and
`CanDownload` is advertised generally. Restricting it to `Type=Book` would be a
gratuitous divergence.

---

## CHANGED `BaseItemDto` — new `Path` field

```
"Path": "/srv/Books/Dune.epub"
```

- Emitted **only** when the request's `Fields` contains `Path`. The client asks
  as `Fields=CanDownload,Path`.
- Omitted (not null) otherwise, per the existing convention that absent means
  "not requested".
- Applies to every item kind, not just books — `Fields=Path` is a general
  request and the metadata editor uses it.

**Why this is required, not cosmetic**: `canPlayItem` reads `item.Path`. With it
absent the players return false and jellyfin-web declines to open the item with
no error, no toast and no network request. The whole feature fails silently on
this one field.

---

## CHANGED `BaseItemDto` for a book

```jsonc
{
  "Id": "…",
  "Name": "Dune",
  "Type": "Book",            // BaseItemKind
  "MediaType": "Book",       // gates all three players (lowercased compare)
  "Path": "/srv/Books/Dune.epub",   // when Fields=Path
  "CanDownload": true,
  "RunTimeTicks": null,      // R8 — null even though position is tracked
  "SeriesName": "Dune",      // from book.series_name
  "IndexNumber": 1,          // from book.series_index
  "ImageTags": { "Primary": "…" }   // iff a cover was extracted
  // MediaSources / MediaStreams: ABSENT (R9)
}
```

`MediaType` and `Type` are both `Book` and both are required: `Type` drives the
grid's card shape, `MediaType` drives player selection.

---

## CHANGED `/UserViews` — `books` collection

A library configured `kind = "books"` appears as:

```jsonc
{ "Name": "Books", "CollectionType": "books", "Type": "CollectionFolder" }
```

`books` is already understood by the deployed bundle.

---

## CHANGED `/Items` query handling

- `IncludeItemTypes=Book` selects book items (via the existing
  `MediaKind::from_wire`).
- `SortBy=SeriesSortName,SortName` groups a series in reading order using
  `book_series` + `book_series_index`.

---

## Explicitly unchanged

- `/Items/{id}/PlaybackInfo` — a book must never reach codec negotiation (R9). A
  `PlaybackInfo` request naming a book returns no `MediaSources`.
- `/Videos/{id}/stream`, HLS, and every transcode path — never valid for a book.
- `/Items/{id}/Images/Primary` — the cover is served by the existing image path
  with no book-specific handling, because it is written into the existing cache
  under the existing role.
