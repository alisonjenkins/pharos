# ADR-0115: The TV line lands on main and stops being a branch

- **Status:** Accepted
- **Date:** 2026-11-10T19:49:54Z
- **Deciders:** Alison Jenkins

## Context

The Android TV app's playback lived on `feat/tv-android-playback`, a branch
that main never took. Main's Android build could not play (spec 038 G6
precondition) and the branch fell behind main: on 2026-11-10 the TV ran a
Dioxus 0.7 build without fixes already on main, and Alison asked why the TV
app was neither merged nor kept up to date. Bringing the branch up to date
took a replay of its commits onto main and a fix for page scripts that
Dioxus 0.8 dropped on the TV (B1310). Every sync costs that again while two
lines exist.

## Decision

We will land the TV line on main and build the TV app from main from then on.

- The TV-only commits (74, relative to main) are rebased onto main and land
  as one merge-queue batch by rebase-merge. No merge of main into anything.
- After the batch lands, `feat/tv-android-playback` takes no new commits;
  TV APKs build from main.
- Hooks main has that the TV page does not implement yet stay as no-ops on
  the Android build (the episode turn, the inert page behind dialogs, the
  flush when the page is hidden), each ported in its own change.
- CI compiling the `mobile` feature, so the Android page cannot rot again,
  is the second half of FR-601 and follows as its own change (T038-601).

## Consequences

- One line: a fix on main reaches the TV build, and no sync is owed.
- Main's web and server changes can now break the Android build. Until CI
  compiles `mobile`, that is caught only by an Android build by hand.
- The no-op hooks are known gaps on the TV, listed above.

## Alternatives considered

- Keep the branch and sync it with main now and then: rejected, it fell
  behind once already and each sync was a replay with conflicts.
- Merge main into the branch: rejected, the merge queue lands by rebase-merge
  only and a merge commit hides the TV-only commits from review.

## References

- spec 038 FR-601, T038-601; B1310, V1280; ADR-0104 (page visualiser),
  ADR-0111.
