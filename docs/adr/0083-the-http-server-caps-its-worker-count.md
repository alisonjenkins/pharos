# ADR-0083: The HTTP server caps its worker count

- **Status:** Accepted
- **Date:** 2026-11-09T10:00:00Z
- **Deciders:** Alison Jenkins

## Context

actix-web runs one single-threaded worker per CPU by default and builds the
whole `App` in each one: every route's `ResourceDef` regex and every
middleware. On 2026-11-09 a jemalloc heap profile of a release build on a
prod-shaped library (15.6k items) put about 5.6 MiB of live heap per worker
there. Production runs on a 44-CPU node, so it ran 44 workers and held about
250 MiB for route tables alone, while Jellyfin idled at ~342 MiB in total on
the same library.

Measured locally (allocated = jemalloc live heap ~70 s after boot; oha,
15 s per cell, rps at 16 / 64 concurrent):

| Workers | Allocated | Latest | Items paging | Item detail | `/System/Info/Public` |
|---|---|---|---|---|---|
| 44 | 341 MiB | 874 / 874 | 133 / 133 | 5761 / 5638 | 46403 / 45354 |
| 16 | 184 MiB | 872 / 896 | 130 / 133 | 5715 / 5560 | 45900 / 44242 |
| 8 | 139 MiB | 873 / 895 | 132 / 133 | 5377 / 5085 | 45187 / 40831 |
| 4 | 117 MiB | 855 / 878 | 129 / 130 | 4727 / 4486 | 39284 / 33004 |

Latest and paging do not depend on the worker count (their work runs on the
tokio pool and Postgres). The fast routes stop gaining between 8 and 16.

## Decision

- `[server].http_workers` sets the worker count. Unset or `0` means one per
  usable CPU, capped at 16.
- The count and its source are published as `pharos_http_workers{source}`
  and logged once at startup (`http workers chosen`).

## Consequences

- Production drops from 44 to 16 workers: about 160 MiB less heap with no
  measured throughput change.
- Going to 8 would save about 46 MiB more at a 7–10% loss of peak throughput
  on the fast routes; that is a trade-off, left to the config option.
- A machine with more than 16 CPUs and a much higher request rate than this
  deployment's can raise `http_workers`.
