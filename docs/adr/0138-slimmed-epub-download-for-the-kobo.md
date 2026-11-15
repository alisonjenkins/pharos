# ADR-0138: A slimmed EPUB copy for clients with a small download budget

- **Status:** Accepted
- **Date:** 2026-11-15T12:00:00Z
- **Deciders:** the project owner (chose a server-side slimmed copy for the Kobo), Claude

## Context

The Kobo cannot take a downloaded file over 32 MB. Its EPUB reader keeps only
8 MB of pictures per book, 2 MB at most per picture. A big EPUB is big because
of its pictures, and the Kobo Libra Colour screen is 1264x1680, so most of
those pixels are never shown.

`GET /Items/{itemId}/Download` sends the stored file through `NamedFile`
(ranges, ETag, correct `Content-Length`). The media share is read-only; the
server already keeps derived files in a cache directory beside it (the
converted Kindle books of ADR-0025's cache, `[server].book_cache_dir`).

Jellyfin's image routes bound a picture with `maxWidth` / `maxHeight`, which the
codebase reads case-insensitively through `ci_query`.

## Decision

We will serve a slimmed copy of an EPUB when the download names a box.

- **Request.** `maxWidth` and/or `maxHeight` on `/Items/{id}/Download`, named
  as Jellyfin's image routes name them. One edge alone leaves the other
  unbounded. Unparseable, zero or over-16384 values read as absent. Without a
  box, or for a file that is not an EPUB, nothing changes.
- **Re-pack.** `book_slim::repack` writes `mimetype` first and stored (OCF),
  then each entry in order. A JPEG, PNG, WebP or GIF larger than the box is
  decoded under the avatar decode limits, oriented, scaled to fit with
  Lanczos3 and encoded in its own format (JPEG quality 80), so no name or
  manifest media type changes. Every other entry, and every picture that fits,
  is copied compressed-byte for compressed-byte with `raw_copy_file`.
- **Cache.** `<book cache dir>/slim/<id>[-<edition>]-<mtime_ns>-<bytes>-<w>x<h>.epub`,
  written to a temporary name and renamed. The decision "the original is the
  answer" is stored as an empty `.orig` file beside it.
- **Fallback.** The original is served when no picture is larger than the box,
  when the copy is not smaller, when no cache directory is configured, and on
  any failure (unreadable zip, no `mimetype`, a stalled queue). Nothing here
  returns a new error status; an unreadable source is still the handler's 404.
- **Bounds.** At most 20,000 entries; pictures over 64 MiB uncompressed, and
  pictures after 1 GiB of decoded pictures in one book, are copied unscaled; a
  decode may claim 12,000 px per edge and 256 MiB; two re-packs run at once on
  blocking threads and a third waits up to 30 s.
- **Signal.** `pharos_book_slim_total{outcome}` and one `book slim:` log line
  per decision (V1427).

## Consequences

- A Kobo client can ask for `maxWidth=1264&maxHeight=1680` and get a book it can
  open. The first request pays the re-pack; later ones cost a `stat`.
- Stale copies of an edited book stay in `slim/` until the cache directory is
  cleared; the book cache has no byte cap or sweeper (the converted-book
  rationale: it regenerates on demand). Revisit if slimmed copies prove large.
- A WebP picture is re-encoded lossless (the `image` crate has no lossy WebP
  encoder); it is kept as is when that is not smaller. A PNG photo stays a PNG
  and so shrinks less than a JPEG would.
- An animated GIF is kept as is, since one frame would destroy it.
- The limits are constants, not config keys; none of the existing book config
  carries a decode budget. Make them config if an operator needs to move them.
- The client must ask. Nothing detects a Kobo on its own.

## Alternatives considered

- **Convert every picture to JPEG.** Smaller for PNG photos, but changes the
  entry name or leaves a media type that lies, so the OPF and every reference
  need rewriting. Not worth the risk for a first version.
- **Slim at scan time.** Writes output for books nobody opens into a cache the
  size of the library; the converted-book design already rejected this.
- **A separate route.** Diverges from the one URL the readers build; the box
  fits the existing image-route vocabulary.

## References

- B1451, V1426, V1427; `crates/pharos-server/src/book_slim.rs`
- ADR-0025 (Kindle conversion, the book cache this sits in)
- `crates/pharos-server/src/avatar.rs` (decode limits and slots this follows)
