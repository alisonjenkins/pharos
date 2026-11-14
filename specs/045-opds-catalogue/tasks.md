# Tasks: an OPDS catalogue for e-readers (045)

**Input**: [spec.md](spec.md), [plan.md](plan.md), ADR-0128. Ids are local to this spec (`T045-NN`). Fixtures are invented; no real titles, authors or paths.

## Credential

- [x] T045-01 `pharos-core`: reader password id, typed parse, shown form, store trait.
- [x] T045-02 Migration `0080_reader_passwords` (sqlite, postgres) and the migrate copy list. Needs a migration number from the queue.
- [x] T045-03 sqlite and postgres store, `AnyStore` dispatch, conformance suite.
- [x] T045-04 `LoginThrottle::verify_with`: the throttle takes any credential check.
- [x] T045-05 Service: create (limit, label, prune, hash, id retry), list, revoke, `authenticate`, verified cache, metrics (FR-010 to FR-014, V1394).

## Catalogue

- [x] T045-06 Shared download response; `Content-Disposition` valid for any name (B1425, V1396).
- [x] T045-07 Catalogue rules and feed XML (FR-001 to FR-004, V1395).
- [x] T045-08 `/opds` routes: auth, access, feeds, search, covers, download, request and download counters (FR-005, FR-006, FR-030).
- [x] T045-09 `it_books::opds`: shape, paging, search, auth, access, download; disarm check on the auth counter.

## Account

- [x] T045-10 RPC: three Session methods, schemaVersion 46 (needs a number from the queue); `it_rpc::reader_passwords`; redaction test covers the new secret.
- [x] T045-11 `pharos-client` wrappers; `it_rpc::client_api` round trip.
- [x] T045-12 UI: E-readers section, SSR tests (FR-020 to FR-022).

## Docs

- [x] T045-13 ADR-0128, invariants V1394-V1396, `docs/kobo-koreader.md`.

## Open

- [ ] T045-14 An end-to-end test of the cover routes with an image cache in the rig.
- [ ] T045-15 Tests for the disabled-account and outside-schedule refusals on `/opds/`.
- [ ] T045-16 After deploy, run the spec's signal queries against a real KOReader session and record their output.
