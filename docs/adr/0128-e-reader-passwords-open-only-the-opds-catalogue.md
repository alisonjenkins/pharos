# ADR-0128: E-reader passwords open only the OPDS catalogue

- **Status:** Accepted
- **Date:** 2026-11-14T12:00:00Z
- **Deciders:** Alison

## Context

Spec 045 adds an OPDS 1.2 catalogue so an e-reader (a Kobo running
KOReader) can browse and download the user's books. The server is reachable
from the internet behind a ModSecurity/CRS WAF.

- KOReader's OPDS client speaks only HTTP Basic auth. It cannot do the
  Jellyfin token flow or Quick Connect.
- An e-reader stores what it is given in a plain settings file, and sends it
  on every request. The account password must never be one of those.
- A cover page asks for dozens of thumbnails at once. Argon2 per request
  would make that slow and would spend the verify slots login shares.
- The login throttle (V174) locks an account after repeated failures. A
  reader left with a revoked password retries for ever.

## Decision

We will give each user **e-reader passwords**, separate from the account
password, that authorise only `/opds/`.

- **Shape.** A password is `<id>-<secret>`: a 6-character public id and a
  16-character secret, from 31 letters and digits with no look-alikes
  (`0 1 i l o`), shown as `abcdef-ghjk-mnpq-rstu-vwxy`. Case, spaces and
  dashes are ignored when typed. The secret is about 79 bits.
- **Storage.** Table `reader_passwords` (id, user, label, hash, created,
  last used, revoked). The secret is stored only as the auth backend's
  argon2 hash. The id picks the row, so a check costs one verify however
  many passwords a user holds. At most 10 live passwords per user; labels
  are 1-64 characters with no control characters.
- **Shown once.** `Session.createReaderPassword` returns the password in a
  `$secret` field and it is never readable again. Listing returns label,
  created and last used. Revoking takes effect on the next request.
- **Scope.** The OPDS scope reads only Basic auth with an e-reader
  password. The account password is refused there, and an e-reader
  password is refused everywhere else (V1394). 401 answers carry
  `WWW-Authenticate: Basic realm="Pharos"`.
- **Throttle.** Checks go through their own `LoginThrottle` instance with the
  login policy (per user and per IP, 429 with `Retry-After`). E-reader
  failures do not count toward the account's lockout, so a stale reader
  cannot lock its owner out of the web UI.
- **Cache.** A verified secret is cached for 15 minutes (at most 1024
  entries, SHA-256 under a random per-process key, constant-time compare).
  The row is still read on every request, so revocation, a disabled account
  and the access schedule apply at once.
- **Last used** is written at most every 5 minutes per password.
- **Revoked rows** are kept 30 days so a refusal logs `revoked`, then pruned
  when the user next creates one.
- **Signals.** `pharos_opds_auth_total{outcome}` and
  `pharos_reader_password_total{action,outcome}`. Logs carry the typed
  username and the outcome, never the password.

## Consequences

- An e-reader can sign in with what it supports, and losing one costs one
  revoke, not an account password change.
- A new credential kind exists. Every future scope must decide whether it
  accepts it; today only `/opds/` does.
- Basic auth sends the password on every request, so the catalogue relies on
  TLS at the proxy. Plain-HTTP LAN use exposes it to the LAN.
- A second throttle means an attacker gets the login budget twice per user:
  once for the account, once for e-reader passwords. The secret's 79 bits
  make that budget irrelevant.
- The migration (`0080_reader_passwords`) and schemaVersion 47 take numbers
  from the queue when this lands.

## Alternatives considered

- **Account password over Basic auth.** Rejected: it would live in the
  reader's settings file and give full account access.
- **API keys (`X-Emby-Token`) in the URL.** KOReader can put a token in the
  catalogue URL, but URLs end up in proxy and WAF logs, and API keys open
  the whole Jellyfin surface.
- **One random secret per password, looked up by hash.** Argon2 hashes are
  salted, so a lookup would verify every row of the user. The public id
  avoids that.
- **Counting e-reader failures toward the account lockout.** Rejected: a
  forgotten reader would keep its owner locked out.

## References

- `specs/045-opds-catalogue/`, V1394-V1396.
- `crates/pharos-core/src/reader_password.rs`,
  `crates/pharos-server/src/services/reader_password.rs`,
  `crates/pharos-server/src/api/opds/`.
- OPDS Catalog 1.2: https://specs.opds.io/opds-1.2
- ADR-0120 (button family) for the account UI.
