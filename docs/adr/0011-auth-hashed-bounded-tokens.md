# ADR-0011: Hashed, bounded-lifetime session tokens + CLI admin bootstrap

- **Status:** Accepted
- **Date:** 2026-07-09T00:00:00Z
- **Deciders:** Alison

## Context

pharos uses Jellyfin's bearer-token auth (ADR-0001): a client authenticates with
username/password and gets an `AccessToken` it presents on every request. As
pharos moves toward public exposure (`prototype.aliflix.…`), two weaknesses in
the naive version had to be fixed: tokens stored in plaintext (a DB read =
account takeover) with no expiry (a leaked token is valid forever), and a
first-admin bootstrap that relied on hardcoded Playwright seed credentials which
must never ship in a release.

## Decision

- **Session tokens are hashed at rest** and carry a **bounded lifetime**; the
  stored form is not the presented secret, and expired tokens are rejected.
- **Admin bootstrap is an explicit CLI**: `pharos admin create-user --name … 
  --admin` (password via `PHAROS_ADMIN_PASSWORD` env, preferred over a flag so it
  stays out of the process list / shell history). `pharos admin reset-password`
  is the out-of-band recovery path (writes the store directly, works even when
  every admin is locked out).
- The Playwright seed users (`SeedPlaywrightUser` / `CreatePlaywrightUser`) are
  **compiled out of release builds** (`#[cfg(debug_assertions)]`) so the
  hardcoded credential cannot reach production.
- Every admin/mutation route is gated on `user.policy.admin` via a
  `require_admin` check; the `route_auth_audit` test enforces that every route is
  either authed or explicitly allow-listed as public-by-design.
- API keys (long-lived, for scripts) are a separate, revocable token class keyed
  by `apikey:{app}` in the token's device id.

## Consequences

- A database disclosure no longer yields usable tokens; a leaked token expires.
- First-run setup is an explicit operator action (`create-user`), not an implicit
  seeded account — safe to expose publicly.
- Image `<img src=…>` GETs and the subtitle `Stream.js` fetch are public-by-
  design (clients can't attach auth headers to them); these are the documented
  exceptions in the auth audit.

## References

- `crates/pharos-server/src/auth.rs`, `api/jellyfin/admin.rs`, `cli.rs`
- `crates/pharos-server/tests/route_auth_audit.rs`
- memory `project_pharos_auth_hardening`
