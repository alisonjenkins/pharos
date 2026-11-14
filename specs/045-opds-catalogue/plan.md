# Implementation plan: an OPDS catalogue for e-readers (045)

**Spec**: [spec.md](spec.md). **ADR**: ADR-0128.

## Shape

| Piece | Where | Holds |
|---|---|---|
| Types and store trait | `pharos-core/src/reader_password.rs` | `ReaderPasswordId`, `TypedReaderPassword::parse`, `format_reader_password`, `ReaderPasswordStore` |
| Table | `pharos-store-sqlx/migrations/{sqlite,postgres}/0080_reader_passwords.sql` | id, user (cascade), label, hash, created, last used, revoked |
| Store | `pharos-store-sqlx/src/reader_password.rs` | sqlite and postgres through `impl_store!`; `AnyStore` dispatch; conformance suite |
| Service | `pharos-server/src/services/reader_password.rs` | create / list / revoke, `authenticate` (Basic parse, throttle, verify, cache), metrics |
| Throttle | `pharos-server/src/auth.rs` | `LoginThrottle::verify_with` so another credential check reuses V174 |
| Catalogue rules | `pharos-server/src/api/opds/catalog.rs` | `openable`, sorting, authors, search words, paging; pure |
| Feed XML | `pharos-server/src/api/opds/feed.rs` | Atom navigation and acquisition feeds, OpenSearch; pure |
| Routes | `pharos-server/src/api/opds/mod.rs` | the `/opds` scope, auth, access, download, covers, request counter |
| Download | `pharos-server/src/api/jellyfin/download.rs` | `serve_item_file` shared with the Jellyfin route; `Content-Disposition` fix (B1425) |
| RPC | `pharos.capnp` (schemaVersion 46), `rpc/session.rs`, `pharos-client` | three Session methods |
| UI | `pharos-ui/src/native/reader_passwords.rs`, `app.rs`, `pharos.css` | the E-readers section |

## Choices

- The catalogue is built in memory from `list_items_cached` filtered by `openable` and `access::visible_items`, so listing, search, counts and paging use one rule set. A library of a few thousand books is a few milliseconds.
- Author keys in URLs are an xxh3 hash of the lowercased name, so no name text reaches a URL or a WAF log.
- Search follows the library search (V558): every word in title, author or series, at most 10 words of 200 characters.
- Covers go through the OPDS scope (Basic auth) and reuse the image cache's JPEG Primary path at widths 480 and 160.

## Gates

- Unit: catalogue rules, feed escaping, password parsing, label rules, cache, outcome labels distinct.
- `it_books::opds`: feed shape parsed back, paging, search, auth, access, download bytes and headers.
- `it_rpc`: create, list, revoke through the server and the client.
- Store conformance on sqlite and postgres.
- Disarm check: removing the `pharos_opds_auth_total` increment turns `only_a_live_e_reader_password_opens_the_catalogue` red.
