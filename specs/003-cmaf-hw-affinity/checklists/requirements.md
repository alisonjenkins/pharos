# Specification Quality Checklist: hardware encoding for CMAF renditions

**Purpose**: Validate specification completeness and quality before planning
**Created**: 2026-07-27
**Feature**: [spec.md](../spec.md)

## Content Quality

- [x] No implementation details (languages, frameworks, APIs)
- [x] Focused on user value and business needs
- [x] Written for non-technical stakeholders
- [x] All mandatory sections completed

## Requirement Completeness

- [x] No [NEEDS CLARIFICATION] markers remain
- [x] Requirements are testable and unambiguous
- [x] Success criteria are measurable
- [x] Success criteria are technology-agnostic (no implementation details)
- [x] All acceptance scenarios are defined
- [x] Edge cases are identified
- [x] Scope is clearly bounded
- [x] Dependencies and assumptions identified

## Feature Readiness

- [x] All functional requirements have clear acceptance criteria
- [x] User scenarios cover primary flows
- [x] Feature meets measurable outcomes defined in Success Criteria
- [x] No implementation details leak into specification

## Notes

- SC-001/SC-002 baselines are MEASURED from production 2026-07-27, not estimated:
  420/423 jobs on CPU at median 3380 ms; NVENC 3 jobs at median 1825 ms.
- The spec deliberately does NOT choose between "wait for the pinned device" and
  "start a new generation" (User Story 3). Both satisfy FR-004; picking one is a
  `plan.md` decision that needs the cost of a mid-film init change weighed
  against stall time.
- One requirement is a guard rather than a feature: FR-001/Story 2 restates the
  constraint the current CPU-only rule exists to enforce. It must be tested at
  least as hard as the throughput win, or this reintroduces issue #114.
