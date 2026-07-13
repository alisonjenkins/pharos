# ADR-0003: SQLite (WAL) default store, Postgres alternative

- **Status:** Accepted — amended by [ADR-0015](0015-postgres-cnpg-deployment.md)
  (the home deployment runs the Postgres backend since 2026-07-11; SQLite
  remains the project default)
- **Date:** 2026-07-09T00:00:00Z
- **Deciders:** Alison

## Context

pharos needs durable storage for its catalogue (media items + probe metadata),
users/tokens, scan state, and user-item data. The deployment target is home
infrastructure: a single server pod, media on NFS, small-to-medium library
(~12k items). Operational simplicity matters more than horizontal scale — there
is no need for a separate database tier, and "one file on a PVC" is the easiest
thing to back up and reason about.

Some deployments (or future multi-replica setups) do want a real networked DB.

## Decision

We support two backends behind one `MediaStore` (+ `UserStore`, `TokenStore`, …)
trait set in `pharos-store-sqlx`: **SQLite is the default**, **PostgreSQL is the
alternative**. Both are driven by `sqlx` with parallel migration trees
(`migrations/sqlite/`, `migrations/postgres/`).

SQLite is opened in **WAL** journal mode with `synchronous=NORMAL`, foreign keys
on, and a **15s `busy_timeout`**. WAL + busy_timeout is what makes concurrent
access safe: request handlers, the background scanner, and even a second
in-process scan share the file without "database is locked" errors, and a
separate maintenance process can touch the same DB file within the timeout
window.

## Consequences

- Zero-dependency deployment: the store is a file on the (RWO) PVC; the server
  pod uses a `Recreate` strategy because the PVC can't be multi-attached.
- The single-writer nature of SQLite means the scanner and request writes
  serialise; busy_timeout absorbs the contention rather than erroring.
- Every schema change must be written **twice** (sqlite + postgres migration),
  and the store impls (`sqlite.rs`, `postgres.rs`) kept in sync — a maintenance
  tax paid on each column addition.
- Postgres remains a tested path but is not what the home cluster runs.

## Alternatives considered

- **Postgres-only:** rejected — adds a DB tier + backup story for a single-user
  home server with no scale need.
- **An embedded KV (sled/redb):** rejected — loses SQL ergonomics and `sqlx`
  compile-time checking; the catalogue is genuinely relational.

## References

- `crates/pharos-store-sqlx/`, `CLAUDE.md`
