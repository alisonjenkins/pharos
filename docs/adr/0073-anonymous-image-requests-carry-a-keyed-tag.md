# ADR-0073: Anonymous image requests carry a keyed tag

- **Status:** Accepted
- **Date:** 2026-11-08T09:30:00Z
- **Deciders:** Alison Jenkins

Supersedes ADR-0040 in part: anonymous item image requests are still
served, but only with a tag this server minted, once enforcement is on.

## Context

ADR-0040 made item images anonymous again so the Jellyfin Android app,
whose image loader sends no credential, shows artwork. The cost it accepted:
anyone who can reach the server can fetch any item's artwork by guessing
its id, because the `ImageTag` the app echoes as `?tag=` was an unkeyed hash
of id, art version and role (B762).

Every image URL a client builds comes from a listing it fetched while signed
in, and carries that listing's tag. So a tag only this server can produce
proves the requester was handed it. ADR-0040 chose this as the follow-up.

Tags are minted in about seventy places (`dto::image_tag_for`, synthesised
series, albums and libraries, live TV, playlists), and some pair a tag with
another item's id (`SeriesPrimaryImageTag` with `SeriesId`, and so on).

## Decision

- A signed tag is the old ("inner") tag followed by 16 hex digits of
  HMAC-SHA256 under a server key over the URL's canonical item id (dashless,
  lowercase), the lowercase role (with the index for a backdrop or chapter
  past the first) and the inner tag, with a versioned domain separator. The inner tag keeps the art version, so a new image still
  changes the URL. Verifying needs only the key and the URL, so a stale tag
  from a cached listing still verifies after the art changes.
- The key is 32 random bytes kept in a new `server_secrets` table
  (migration 0070), created on first boot and copied by the sqlite to
  postgres migration. A stored key of the wrong length stops the server from
  starting instead of rotating the key.
- The DTO builders keep minting inner tags. A response layer signs every
  `application/json` body that mentions `ImageTag`, pairing each tag field
  with the id field a client builds the URL from
  (`pharos_jellyfin_api::image_tag_sig::sign_image_tags`). One table instead
  of a signer at every tag site.
- The item image routes (`/Items/{id}/Images/{role}[/{index}]`, GET and HEAD)
  and both chapter routes check the tag of a request that has no
  credential. `pharos_image_tag_check_total{verdict,enforced}` counts
  `valid`, `invalid` or `absent`. A request with a credential keeps the
  library-access check (V189) and is not tag-checked.
- The refusal is behind `[server].image_tags_enforced`, off by default.
  Rollout: deploy, read the counter, turn it on once `invalid` and `absent`
  have gone quiet (V712).
- The native API's image URLs carry the same signed tag, written by the
  RPC encoders with the same key, so both APIs agree on a tile's tag.
- Exempt: person portraits (a 302 to the provider's public URL), user
  avatars and live TV channel logos.

## Consequences

- With enforcement on, artwork can no longer be fetched by guessing an id.
  ADR-0040's open B249 image half closes for every client that echoes tags.
- Every tag changes once on deploy, so clients re-download artwork they had
  cached for up to seven days.
- Signing re-parses and re-serialises each GET JSON body that carries
  tags: one extra pass over the listing. Not measured before merge. The
  layer sits inside RedMetrics, so `http_request_duration_seconds{path="/items"}`
  shows the cost after deploy, and `pharos_image_tag_sign_total{outcome}`
  counts the bodies signed.
- A client that drops `tag=`, or builds an image URL from an id alone, gets
  401 once enforcement is on. The counter shows this before it happens.
- Someone who once held a listing keeps a working tag until the key
  rotates. Rotating means deleting the `server_secrets` row, which
  invalidates every cached tag.

## Alternatives considered

- Sign at each tag site: about seventy edits. Any site missed fails only
  under enforcement, and the id paired with a parent tag is easy to get
  wrong. Rejected for one table with one test.
- MAC over the art version instead of carrying the inner tag: a tag cached
  before an art change would stop verifying. Rejected.
- Enforce on the first deploy: cached listings and apps still holding unkeyed
  tags would lose artwork, repeating B286. Rejected for count-first rollout.
- Derive the key from `server_id`: `/System/Info/Public` publishes it.
  Rejected.

## References

B249, B286, B762, V189, V225, V712 in `specs/001-pharos-baseline/`;
ADR-0031, ADR-0034, ADR-0040; `crates/pharos-jellyfin-api/src/image_tag_sig.rs`,
`crates/pharos-server/src/middleware/sign_image_tags.rs`,
`crates/pharos-server/src/api/jellyfin/images.rs`.
