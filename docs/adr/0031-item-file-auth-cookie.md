# ADR-0031: Item file routes require auth; jellyfin-web authenticates them with a narrowly scoped cookie

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

## Decision

We will require a signed-in bearer on every item file route (V189) and accept
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
- A native client that fetches images with neither a header nor `api_key`
  gets 401.
- The Dioxus UI's `pharos_session` cookie is `Path=/pharos`, so its images
  need their own mechanism (T145).
- Switching users in jellyfin-web without signing out keeps the previous
  user's cookie until the next sign-in.

## Alternatives considered

- Patch the jellyfin-web bundle to add `api_key` to image URLs: three apiclient
  sites plus the SDK, re-done on every jellyfin-web upgrade. Rejected.
- Keep images anonymous: leaves B249 open on a public server. Rejected.
- Accept the cookie on every route (P24 as it was): exposes writes to
  cross-subdomain requests (B250). Rejected.

## References

B247–B250, V189–V191 in `specs/001-pharos-baseline/`;
`crates/pharos-server/src/api/jellyfin/auth_extractor.rs`.
