# Contract: `GET /MediaSegments/{itemId}`

**Status for this feature**: **UNCHANGED**. Recorded so a regression here is
visible, and so the verified-healthy finding is not re-litigated.

## Request

```
GET /MediaSegments/{itemId}
    ?includeSegmentTypes=Intro
    &includeSegmentTypes=Outro
    &includeSegmentTypes=Preview
    &includeSegmentTypes=Recap
    &includeSegmentTypes=Commercial
```

Observed verbatim from Jellyfin Android TV 0.19.9
(`jellyfin-sdk-kotlin`/OkHttp) in production on 2026-07-27.

- `itemId` is the canonical dashless-32-hex wire id (V41).
- `includeSegmentTypes` arrives **repeated**, not comma-joined. Repeated keys are
  merged into one comma-joined value before deserialization (the B20 fix); a
  scalar field would otherwise 400. Both spellings must keep working (V69).
- An absent or blank filter returns every segment type.
- Requires authentication.

## Response

`200` with the standard list envelope:

```json
{
  "Items": [
    {
      "Id": "<uuid, 32 hex>",
      "ItemId": "<item wire id>",
      "StartTicks": 0,
      "EndTicks": 900000000,
      "Type": "Intro"
    }
  ],
  "TotalRecordCount": 1,
  "StartIndex": 0
}
```

- `Id` MUST be a valid UUID. A non-UUID string crashes the strict Kotlin client
  mid-playback (B69); it is derived deterministically from `(item_id, key)` so it
  is stable across requests and cannot be malformed by construction.
- `Type` is a member of the Jellyfin `MediaSegmentType` enum. An out-of-set value
  must never be emitted (V39).
- Ticks are 100 ns units on the episode's own timeline.

## Invariants this feature must not break

1. **Intro and Outro travel the same path.** They are built by the same function,
   filtered by the same allow-list, and serialized by the same DTO. A change that
   makes one work and the other not is a defect by construction.
2. **Chapters win over detection for the same type.** An author-labelled chapter
   is exact; a detected range is inferred. When a chapter already supplied a type,
   the detected range for that type is skipped.
3. **An empty result is a valid answer**, not an error. A show with no shared
   opening returns no Intro and the client shows no button.

## Verification

Both kinds delivered for an episode that has both:

```
GET /MediaSegments/{id}?includeSegmentTypes=Intro&includeSegmentTypes=Outro
→ 200, Items contains one Type:"Intro" and one Type:"Outro"
```

Covered in-tree by `crates/pharos-server/tests/jellyfin_media_segments.rs`.
