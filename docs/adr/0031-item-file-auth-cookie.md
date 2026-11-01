# ADR-0031: Subtitle and font routes require auth, item images stay public, and the JellyfinAuth cookie is narrowly scoped

- **Status:** Accepted
- **Date:** 2026-11-01T23:30:00Z
- **Deciders:** Alison Jenkins

## Context

pharos is reachable from the internet. Item images, subtitles and fonts were
served to anonymous requests, so anyone could fetch them by id and the library
policy (V187) could not apply (B249). Requiring a bearer breaks jellyfin-web
unless its element fetches can authenticate: it builds most image URLs inside
its bundled apiclient and SDK without a token, and `<img>`, `<track>` and font
fetches cannot send a header.

A `JellyfinAuth` cookie already existed (P24) but was read inside
`extract_token`, so it authenticated every route and every method (B250). A
browser sends a cookie on any same-site request, and sibling subdomains on the
same registrable domain are same-site, so `SameSite=Strict` does not stop a
page on another subdomain from making writes with it.

Native Jellyfin clients do not send a token on artwork requests:
jellyfin-androidtv, Swiftfin and Findroid load `/Items/{id}/Images/*` through
Coil or Nuke with no `api_key` and no auth header, and the Kotlin and Swift
SDK URL builders add none (read in their sources, 2026-11-01).

## Decision

Item image GET/HEAD stay anonymous, as Jellyfin's are, and apply V187's 404
for a hidden item whenever the request names a bearer that resolves. We will
require a signed-in bearer on the subtitle, attachment and font routes (V189), and accept
the `JellyfinAuth` cookie as that bearer only on GET and HEAD requests to
routes that serve item bytes to element URLs (V191): images, subtitles,
fonts, streams, HLS and trickplay. Every write and every JSON API route takes
a header or `api_key` only. Sign-in sets the cookie HttpOnly, SameSite=Strict,
Path=/, Secure on https; logout clears it. PlaybackInfo `DeliveryUrl`s carry
the caller's `api_key` for clients without the cookie.

## Consequences

- jellyfin-web posters and subtitle tracks work unmodified.
- A cross-subdomain page can at most make the browser fetch an item's bytes
  as the viewer, which it cannot read back across origins. It cannot change
  data, but a GET of an HLS playlist or segment does start transcode work and
  fill the segment cache, so it can spend the server's encoder time.
- Responses on these routes are `Cache-Control: private`, so a shared cache
  in front of pharos cannot replay a cookie-authenticated answer to an
  anonymous request for the same token-free URL.
- Native clients keep their artwork.
- Anyone who can reach the server can still fetch an item's artwork by id.
  Ids are sequential, so the posters of the whole library are enumerable;
  subtitles, fonts, streams and metadata are not. A signed-in, folder-restricted
  user who sends their token cannot fetch a hidden item's artwork, but one who
  drops the token can. Closing that needs a token on image URLs that native
  clients would send (T145).
- Switching users in jellyfin-web without signing out keeps the previous
  user's cookie until the next sign-in.

## Alternatives considered

- Patch the jellyfin-web bundle to add `api_key` to image URLs: three apiclient
  sites plus the SDK, re-done on every jellyfin-web upgrade. Rejected.
- Require auth on images too: blanks artwork in every native client listed
  above. Rejected for now (T145).
- Accept the cookie on every route (P24 as it was): exposes writes to
  cross-subdomain requests (B250). Rejected.

## References

B247–B250, V189–V191 in `specs/001-pharos-baseline/`;
`crates/pharos-server/src/api/jellyfin/auth_extractor.rs`.
