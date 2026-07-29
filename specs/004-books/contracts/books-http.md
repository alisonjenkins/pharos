# HTTP contract: native book support

Only the deltas. Everything else on these endpoints is unchanged.

Authority for every requirement below is the deployed jellyfin-web bundle
(see [research.md](../research.md) R2), not the Jellyfin OpenAPI document — where
they disagree, the shipped client wins, because it is the acceptance test.

**Revised 2026-07-29**: the book-item shape now says empty/0 rather than
absent/null (R9), and `PlaybackInfo` moved out of "explicitly unchanged" into a
contract of its own (FR-010).

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
- The parameter and the field name are matched in **every spelling a client
  dialect may send** — `Fields`/`fields`, `Path`/`path` (V69). A camelCase
  spelling silently ignored would disable the entire feature for that client,
  which is a recurring bug class here.
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
  "RunTimeTicks": 0,         // R8/R9 — 0, NOT null; the field is not optional
  "MediaSources": [],        // R9 — EMPTY, not absent
  "MediaStreams": [],        // R9 — EMPTY, not absent
  "SeriesName": "Dune",      // from book.series_name
  "IndexNumber": 1,          // from book.series_index
  "ImageTags": { "Primary": "…" }   // iff a cover was extracted; no Backdrop/Thumb
}
```

`MediaType` and `Type` are both `Book` and both are required: `Type` drives the
grid's card shape, `MediaType` drives player selection.

**On empty rather than absent**: the goal is that a client has nothing to request
a stream for, and `[]` achieves that completely. Omitting the arrays would mean
adding `skip_serializing_if`, and `dto.rs:420` records why that is forbidden —
array fields are default-empty because jellyfin-web iterates them without null
guards, and absent ones throw `Symbol.iterator` TypeErrors during view init.
Likewise `RunTimeTicks` is `u64`, so `null` is not available without changing the
field for every item kind.

---

## CHANGED `POST|GET /Items/{itemId}/PlaybackInfo` — books yield no source

The route exists for any item id (`items.rs:105-106`, handler at `items.rs:2081`).
A book must leave it **without having entered codec negotiation**:

```jsonc
{ "MediaSources": [], "PlaySessionId": "…" }
```

No device-profile evaluation, no direct-play/transcode decision, no
`TranscodingUrl`. FR-010 exists because this rule previously lived only in this
document's "explicitly unchanged" section, which meant nothing enforced it.

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
  `book_series` + `book_series_index`. A book with no series index sorts last,
  not as index zero.

---

## Explicitly unchanged

- `/Videos/{id}/stream`, HLS, and every transcode path — never valid for a book.
  Nothing needs to reject a book explicitly, because with `MediaSources: []` no
  client constructs such a URL.
- `/Items/{id}/Images/Primary` — the cover is served by the existing image path
  with no book-specific handling, because it is written into the existing cache
  under the existing role.
