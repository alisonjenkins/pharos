# Specification quality checklist: Pharos-styled animated subtitles (028)

**Purpose**: Check that the spec is complete and of good quality before planning
**Created**: 2026-11-06T00:55:00Z
**Feature**: [spec.md](../spec.md)

## Content quality

- [x] Written around user value: what a viewer sees, how it suits the device, and what an operator can find
- [x] All mandatory sections are filled in
- [ ] No implementation details. Partial: the FRs name crates, routes and tags because the design requires the ASS output and the worker op. The *why* lives in the problem statement and ADR-0067.

## Requirement completeness

- [ ] No [NEEDS CLARIFICATION] markers remain. One is left, FR-015 (item scope). It is with Alison, and the genre gate is the default until she answers.
- [x] Requirements are testable and unambiguous (each maps to a V id or a task test)
- [x] Success criteria are measurable (SC-001 to SC-004)
- [x] Edge cases are listed
- [x] Scope is bounded (out of scope lists design phases 2-4)
- [x] Dependencies and assumptions are named (spec 026, ADR-0057, the `audio_analysis` pattern, T86 segments)

## Feature readiness

- [x] Every FR has an acceptance path (user stories plus V510-V517)
- [x] Manual gates are marked (SC-001 needs the G08 in daytime; SC-002 needs labelling)
