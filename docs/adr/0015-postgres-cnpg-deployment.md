# ADR-0015: CNPG Postgres for the home deployment; zero-downtime rolling deploys

- **Status:** Accepted
- **Date:** 2026-07-13T00:00:00Z (decision live 2026-07-11)
- **Deciders:** Alison

## Context

ADR-0003 chose SQLite (WAL) as the default store, and the home cluster ran it
on an RWO PVC with a `Recreate` deployment strategy — every deploy was a hard
gap: the old pod had to fully stop (PVC single-attach) before the new one
started, killing active playback and SyncPlay groups on each of the many daily
auto-deploys (ADR-0010). SQLite's single-writer model also forecloses ever
running two pods, even briefly.

Meanwhile SyncPlay grew durable state (ADR-0016) whose whole point is surviving
a deploy — pointless if the deploy itself drops every connection for ~a minute.

## Decision

The **home deployment** switches its store to **PostgreSQL**, provisioned by
**CloudNativePG** (CNPG, `pharos-db` cluster in the pharos namespace), and the
chart's deployment strategy becomes **`RollingUpdate` with surge** (a second
pod briefly overlaps the draining one), giving zero-downtime deploys.

- **SQLite remains the project default** (ADR-0003 stands): zero-dependency
  single-node installs are still the documented base case. The chart *forces*
  `Recreate` whenever the configured database URL is SQLite — RollingUpdate
  with a single-writer file DB would corrupt or deadlock.
- The migration (2026-07-11) copied 289k rows sqlite → postgres with the server
  down, verified counts, then flipped `config.database.url`. The **sqlite PVC
  is retained** (`helm.sh/resource-policy: keep`, reclaim `Retain`) as a
  rollback path.
- `replicaCount` stays 1 in steady state: on a single-node cluster a second
  steady replica buys no HA. The value of Postgres here is the deploy overlap
  window — during which **two replicas genuinely run concurrently**, which is
  what forced the SyncPlay distribution work (ADR-0016).

## Consequences

- Deploys no longer interrupt playback or SyncPlay; CI-to-live is invisible to
  viewers (with ADR-0016 handling the group-state handover).
- pharos now has a DB tier to operate (CNPG handles failover/backups), the
  cost ADR-0003 originally avoided — accepted now that multi-replica overlap
  is a hard requirement.
- Every schema change continues to be written twice (sqlite + postgres
  migration trees) — unchanged from ADR-0003, but now the postgres tree is
  what production actually runs, so it is no longer the less-tested path.
- Rollback to the sqlite PVC is possible but loses rows written since the
  migration; it is a disaster hatch, not a routine path.

## References

- ADR-0003 (SQLite default — amended by this ADR for the home deployment)
- ADR-0016 (SyncPlay durability + multi-replica distribution)
- `charts/pharos/values.yaml` (`strategy`, `replicaCount`), `templates/deployment.yaml`
- memory `project_pharos_zero_downtime_postgres`
