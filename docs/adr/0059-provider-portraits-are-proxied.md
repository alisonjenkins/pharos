# ADR-0059: Provider portraits are fetched by the server and served from its origin

- **Status:** Accepted
- **Date:** 2026-11-04T00:00:00Z
- **Deciders:** Alison Jenkins

## Context

Title pages show each actor's photo (V295). The photos come from TMDB
(`image.tmdb.org`) and TVDB (`artworks.thetvdb.com`) and are stored as a
person's `thumb_url`. The person image route answered a remote `thumb_url`
with a 302 to the provider, which Jellyfin clients follow. The native UI's
Content-Security-Policy allows images from `'self'` only, so the browser
refuses the redirected image and the cast rail shows no photos.

A `thumb_url` also comes from NFO files, which anyone with write access to a
library can edit. Fetching whatever address a `thumb_url` names would let them
make the server request internal addresses.

## Decision

We will fetch provider portraits on the server and pass the bytes through from
`/Items/{person}/Images/Primary`.

- Only https URLs on `image.tmdb.org` and `artworks.thetvdb.com` are fetched
  (`portrait::is_proxied`). Every other remote `thumb_url` keeps the 302.
- The fetch follows no redirects, times out after 10 s, requires an `image/*`
  content type, stops reading past 4 MiB and runs at most 8 at once. A failure
  answers 404 and the UI draws the actor's initials.
- A served portrait is cacheable by the client for 7 days. The server keeps no
  copy.
- `ItemDetail.cast` carries an image URL only for a proxied portrait, so the
  UI never asks for one it cannot load.

## Consequences

- Photos load under the existing CSP; the CSP does not widen to provider hosts.
- Each uncached view of a portrait costs one upstream request. A title page
  asks for up to 40 at once; the concurrency cap queues the rest.
- A new provider's image host needs adding to `PORTRAIT_HOSTS` before its
  portraits show.

## Alternatives considered

- Add the provider hosts to the CSP `img-src`: the browser then contacts the
  providers directly, leaking what a viewer browses, and a self-hosted UI has
  to track their hosts.
- Download portraits into the image cache during enrichment: durable and
  offline, but adds thousands of files and a cache lifecycle for images most
  viewers never open.
- Proxy any `thumb_url`: lets NFO content steer server requests.

## References

- `specs/001-pharos-baseline/invariants.md` V295, `bugs.md` B355
- `crates/pharos-server/src/portrait.rs`
- `crates/pharos-server/src/api/jellyfin/images.rs` (person branch)
