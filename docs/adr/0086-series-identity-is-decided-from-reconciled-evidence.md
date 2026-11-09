# ADR-0086: Series identity is decided from reconciled evidence, and episodes inherit it

- **Status:** Proposed
- **Date:** 2026-11-09T14:30:00Z
- **Deciders:** Alison (pending review)

## Context

Shows remade under one name (Battlestar Galactica 1978/2004, The Flash
1990/2014, Cosmos 1980/2014, Life on Mars UK/US) are confused in the deployed
library: 176 episode rows carry the other show's metadata (spec 033, read-only
queries 2026-11-09T14:09Z). LIB-C11 already keys a series on its folder, so
folders never merge. The faults are elsewhere:

- the show year written in file names (`Battlestar Galactica (2003) - S01E01`)
  is ignored, so a bare folder searches with no year and the matcher
  tie-breaks two exact-name candidates by provider order (older show wins);
- every episode resolves its own provider record, by a fresh series search or
  by its NFO id, with no check that it belongs to the show the folder matched;
- one folder can hold two shows (The Flash), which a folder key cannot express;
- `tvshow.nfo` is wrong as often as the search (Cosmos, Life on Mars, Dragon
  Ball, Forever), so neither source can simply be trusted.

#1055 (versions) folds copies within one folder by season and episode, so a
wrong identity there would delete a row that is a different show's episode.

## Decision

We will decide a series' identity once, at the series level, from reconciled
evidence, and make every episode inherit it:

1. **Stated show year per file**: folder `(YYYY)` ▸ file name `Show (YYYY)`
   prefix ▸ release folder; a bare folder takes its files' consensus.
   `tvshow.nfo`'s year and ids are evidence, never identity (V890).
2. **Split a folder by stated year** when its files disagree; the unyeared
   group keeps the plain folder key so existing wire ids hold. Split keys are
   `"{folder}\u{1f}{YYYY}"`, stored in a new `media_items.series_key` column
   backfilled to today's key (V893).
3. **No silent tie-break**: two exact-name candidates without a separating year
   are scored by episode fit (season/episode coverage, filename titles, an NFO
   vote) and either win by a clear margin or are stored as `ambiguous` for a
   manual Identify. Manual matches are never re-asked (V891).
4. **Episodes take metadata only from their series' pinned match**; the
   per-episode series search goes, and an episode NFO id naming another series
   is set aside with its text and stills (V892, extends V468).
5. **Copies never fold across series**: #1055's fold also requires one series
   key, equal stated show years and equal provider series ids where both are
   known (V894).
6. **Repair by dry run, then apply**: `[metadata] series_identity_repair`
   defaults to `"dry_run"`; `"apply"` re-matches only shows with conflicting
   evidence and re-fetches only episodes whose provider show disagrees with
   their series. No row is deleted.

## Consequences

- Easier: a show's episodes always agree with the show; re-matching a show
  (automatically or by Identify) fixes all its episodes at once; one class of
  wrong-episode bugs (B931, B933) goes with the per-episode search.
- Harder: an episode cannot get provider metadata until its show is matched,
  so a fresh library fills in show by show rather than file by file.
  Disambiguation costs up to three extra episode-list fetches per ambiguous
  show, once, cached.
- Risk: a stray unyeared file in a yeared folder could split off as its own
  show; the filename-title check re-joins it when it names an episode of the
  yeared show, and every split is logged and listed on Manage → Integrity.
- Follow-up: the split Flash 2014 group gets a new series wire id; series-level
  favourites stay with the unyeared group.

## Alternatives considered

- **Trust `tvshow.nfo` ids first.** Wrong on 4 of the 6 confused folders'
  evidence; would fix BSG and The Flash and break Cosmos and Life on Mars.
- **Trust the search, add the filename year only.** Fixes BSG; leaves Cosmos and
  Life on Mars (episode NFO ids) and B933 (per-episode searches) broken.
- **Prefer the newer show on a tie.** Swaps which half of the pairs is wrong.
- **Never split; only flag mixed folders.** Simpler and touches no wire ids, but
  leaves The Flash's 62 2014 episodes under one 1990 match until the files are
  moved. Kept as the fallback if Alison prefers it (spec 033, open question 1).

## References

- Spec 033 (`specs/033-series-identity/spec.md`).
- LIB-C11 (`pharos-core` `SeriesInfo::series_key`), B557 (`match_best`), B558 /
  V468 (foreign NFOs), #1055 / ADR 0079 (episode copies as versions).
