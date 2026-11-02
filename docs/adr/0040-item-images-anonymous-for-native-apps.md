# ADR-0040: Item images answer anonymous requests again; a credential still applies the library policy

- **Status:** Accepted
- **Date:** 2026-11-02T21:00:00Z
- **Deciders:** Alison Jenkins

Supersedes ADR-0031 in part: its image decision only. Subtitles, fonts and
the `JellyfinAuth` cookie scope stay as ADR-0031 decided.

## Context

ADR-0031 made every item file route require a signed-in bearer and accepted
that "a native client that fetches images with neither a header nor
`api_key` gets 401". In practice that client is the Jellyfin Android app:
its image loader (okhttp) sends no credential, so after the change every
poster, backdrop and thumb it asked for answered 401 (B286). Over the 24h
before the fix the access log shows 186 image 401s, all from okhttp, and
398 browser image requests, all 200 through the cookie.

Each of the app's image URLs carries `?tag=<ImageTag>`, so a tag only a
signed-in listing hands out could authorize them. Today's tags are an
unkeyed hash of item id, art version and role, so they are guessable, and
pharos has no persisted server secret to key them with.

## Decision

We will serve item image GET and HEAD requests (item, chapter and the image
list) to anonymous callers, as Jellyfin does (V225). A request that carries a
credential that resolves still gets the library policy (V187): a hidden item
is 404. A credential that does not resolve is treated as anonymous.
Subtitles, attachments and fonts still require a bearer (V189); their
`DeliveryUrl`s carry the caller's token.

## Consequences

- The Android app, and any other client whose image loader sends no
  credential, shows artwork again.
- Anyone who can reach the server can fetch an item's artwork by its
  sequential id, as before ADR-0031. B249's image half is open again.
- A folder-restricted user who drops their token sees any item's artwork.
- Anonymous image requests log at DEBUG and count nowhere, so their rate is
  read from the access log, not `pharos_item_access_denied_total`.

## Alternatives considered

- Keyed image tags: authorize an anonymous image request whose `tag=` is a
  MAC under a server key over the item, role and art version. Keeps
  ADR-0031's intent for clients that echo the tag. Needs a persisted server
  secret and a change at every tag site. Chosen as the follow-up, not the
  hotfix.
- Keep ADR-0031 unchanged: leaves the Android app without artwork. Rejected.

## References

B249, B286, V187, V189, V225 in `specs/001-pharos-baseline/`; ADR-0031;
`crates/pharos-server/src/api/jellyfin/images.rs`.
