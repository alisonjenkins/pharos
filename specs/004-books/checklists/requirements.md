# Specification Quality Checklist: native book support

**Purpose**: Validate specification completeness and quality before proceeding to planning
**Created**: 2026-07-29
**Feature**: [spec.md](../spec.md)

## Content Quality

- [x] No implementation details (languages, frameworks, APIs) — **deliberate deviation, see Notes 1**
- [x] Focused on user value and business needs
- [x] Written for non-technical stakeholders — **partial, see Notes 1**
- [x] All mandatory sections completed

## Requirement Completeness

- [x] No [NEEDS CLARIFICATION] markers remain
- [x] Requirements are testable and unambiguous
- [x] Success criteria are measurable
- [x] Success criteria are technology-agnostic (no implementation details) — **deliberate deviation, see Notes 1**
- [x] All acceptance scenarios are defined
- [x] Edge cases are identified
- [x] Scope is clearly bounded
- [x] Dependencies and assumptions identified

## Feature Readiness

- [x] All functional requirements have clear acceptance criteria
- [x] User scenarios cover primary flows
- [x] Feature meets measurable outcomes defined in Success Criteria
- [x] No implementation details leak into specification — **deliberate deviation, see Notes 1**

## Notes

### 1. The three "no implementation details" items are marked complete under a stated deviation

They are not satisfied in the generic sense and cannot be for this feature. The
spec names wire fields (`Path`, `MediaType`, `MediaSources`, `RunTimeTicks`),
an endpoint (`GET /Items/{id}/Download`) and a client (`jellyfin-web`).

That is the requirement, not a leak. Constitution I ("wire compatibility is the
product", NON-NEGOTIABLE) makes an unmodified third-party client the acceptance
test, so a book that opens is *defined* by exact field names and spellings that
client checks. Restating FR-003 as "the item exposes enough information for a
reader to identify it" would be untestable and would have hidden the single
highest-risk fact in the feature — that a missing `Path` fails silently.

Recorded as a deviation rather than silently ticked, so a future reader knows it
was a decision.

### 2. Iteration history

**Iteration 1** (initial spec, 2026-07-29): all items passed except the ones in
Note 1.

**Iteration 2** (post-`/speckit-analyze` refinement, 2026-07-29): three items
had been passing incorrectly and were fixed in the spec:

| Item | Why it actually failed | Fix |
|---|---|---|
| Requirements are testable and unambiguous | FR-008/SC-004 demanded a JSON shape that cannot be produced (`RunTimeTicks` is a non-optional integer, `MediaSources` a non-optional array), and FR-006 required rasterising a PDF page with a parser that cannot rasterise. A requirement that cannot be met is not testable | FR-008/SC-004 restated as empty/0; FR-006 narrowed to page one's embedded image |
| Success criteria are measurable | SC-003's "≥95% of covers" had no stated means of measurement, and SC-002's "500 files" was a figure nothing exercised | SC-003 now requires the rate be observable from the running server; SC-005 added; SC-002 restated size-independently as a count of zero |
| All acceptance scenarios are defined | US4 and US5 had no independent test line, unlike US1–US3 | Added |

**Iteration 3**: not needed — no items failing after iteration 2 apart from the
standing deviation in Note 1.

### 3. Outstanding — not spec defects

`/speckit-analyze` also found six plan/tasks-level issues that this checklist
does not cover and that remain open: I2 (the claim that the compiler enumerates
every match site is false — 34 production sites use `matches!`/`==` on item
kind, including the one that decides `MediaType`), I4, I5, U1, D1, and the call
site behind G4. They need `tasks.md` amendments, listed in spec.md §Revision log
under "Downstream impact".
