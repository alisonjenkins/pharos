# Feature specification: an OPDS catalogue for e-readers (045)

**Created**: 2026-11-14T12:00:00Z
**Status**: Built (see [tasks.md](tasks.md)).
**Scope**: An OPDS 1.2 catalogue of the user's books under `/opds/`, opened by per-user e-reader passwords over HTTP Basic auth, and an account screen to manage them.
**Decision record**: ADR-0128. **Bugs**: B1425. **Verification**: V1394-V1396.

---

## Problem statement

- The user reads on a Kobo Libra Colour with KOReader. Today the only way to get a book from Pharos onto it is to download it on a computer and copy it over USB.
- KOReader has an OPDS client, but it speaks only HTTP Basic auth. Pharos has no OPDS feed and no credential an e-reader may safely hold.
- The server is public on the internet, behind a ModSecurity/CRS WAF. The account password must never be stored on the reader.

## User stories

1. As a reader, I add Pharos to KOReader once, with a URL, my username and a password made for this reader.
2. As a reader, I browse all my books by title, the newest ones, by author and by library, and search by title, author or series.
3. As a reader, I download a book in a format the reader opens, with its cover shown in the list.
4. As a parent, my child's reader shows only the books their account may see.
5. As a user, I list my e-reader passwords with when each was made and last used, and revoke one I lost.
6. As the operator, one query shows how many catalogue requests, sign-ins and downloads succeed or fail, and why.

## Requirements

### Catalogue

- FR-001. `/opds/` serves OPDS 1.2 Atom feeds: a root navigation feed linking All books (paged, by title), Recently added, By author, By library (only with more than one Books library) and search; an OpenSearch description at `/opds/opensearch.xml` with template `/opds/search?q={searchTerms}`.
- FR-002. Only `Book` items are listed, and only those a reader can open from the link (V1395): never `BookFormat::Unreadable` (Kindle DRM among them), never a Kindle file while its EPUB conversion is off, never a remote-origin book, never a file sent as `application/octet-stream`.
- FR-003. Each entry carries title, author, summary, updated, language when known, cover and thumbnail links (`http://opds-spec.org/image`, `.../image/thumbnail`) from the image cache, and one acquisition link with the format's MIME type. A DRM-free Kindle file is offered as `application/epub+zip`.
- FR-004. Feeds page at 50 entries with `first`, `previous`, `next` and `last` links.
- FR-005. Library access and parental controls apply as in `services::access`, to listing, search, counts, covers and downloads.
- FR-006. A download streams the file (GET and HEAD, ranges as the Jellyfin download does) with a `Content-Disposition` valid for any file name (V1396).

### E-reader passwords

- FR-010. A user makes e-reader passwords with a label. Each is shown once, stored only as an argon2 hash, listed with label, created and last used, and revocable. At most 10 live per user.
- FR-011. An e-reader password opens only `/opds/`, over Basic auth. The account password never opens `/opds/` (V1394).
- FR-012. Checks are rate-limited and locked out with the login policy, on their own throttle, and never lock the account itself.
- FR-013. A refused request answers 401 with `WWW-Authenticate: Basic realm="Pharos"`, 429 with `Retry-After` when throttled, 403 outside the access schedule, 503 when the store is down.
- FR-014. Logs carry the typed username and the outcome, never the credential.

### Account UI

- FR-020. Settings → Account gains an "E-readers" section: a label input, the new password shown once with a copy button, the catalogue URL and the username; the list with label, created and last used, and a confirmed revoke.
- FR-021. Buttons are the family's pills (ADR-0120), no CSS `:has()`, and a phone held upright shows every control.
- FR-022. Create, list and revoke go through the native RPC (`Session.readerPasswords`, `createReaderPassword`, `revokeReaderPassword`).

### Observability

- FR-030. `pharos_opds_requests_total{route,outcome}`, `pharos_opds_auth_total{outcome}`, `pharos_opds_downloads_total{format}` and `pharos_reader_password_total{action,outcome}`, every label from a `label()` method and asserted distinct.

## Out of scope

- OPDS 2.0 (JSON), page streaming (OPDS-PSE), reading-position sync with KOReader.
- Audiobooks and comics outside Books libraries.
- Any other Basic-auth route.

## Signals

- Sign-in failures by reason: `sum by (outcome) (rate(pharos_opds_auth_total[5m]))`.
- Catalogue errors: `sum by (route, outcome) (rate(pharos_opds_requests_total{outcome!="ok"}[5m]))`.
- Downloads: `sum by (format) (increase(pharos_opds_downloads_total[1d]))`.
- Refused sign-ins with the username and reason: `{namespace="pharos"} |= "opds credential refused"` (accepted ones log at debug).
