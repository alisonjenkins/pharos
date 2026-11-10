# ADR-0086: Series identity is decided from reconciled evidence, and episodes inherit it

- **Status:** Accepted
- **Date:** 2026-11-09T14:30:00Z
- **Deciders:** Alison (open questions answered 2026-11-09T17:27Z)

## Context

Shows remade under one name (a 1970s show and its 2000s remake, a 1990s show
and its 2010s remake, a UK series and its US remake) are confused in the
deployed library: episode rows carry the other show's metadata (spec 033,
read-only queries 2026-11-09T14:09Z). LIB-C11 already keys a series on its folder, so
folders never merge. The faults are elsewhere:

- the show year written in file names (`Show A (2003) - S01E01`)
  is ignored, so a bare folder searches with no year and the matcher
  tie-breaks two exact-name candidates by provider order (older show wins);
- every episode resolves its own provider record, by a fresh series search or
  by its NFO id, with no check that it belongs to the show the folder matched;
- one folder can hold two shows (a 1991 series and its 2015 remake), which a folder key cannot express;
- `tvshow.nfo` is wrong as often as the search (a remake's NFO ids on the
  original's files, among others), so neither source can simply be trusted.

#1055 (versions) folds copies within one folder by season and episode, so a
wrong identity there would delete a row that is a different show's episode.

## Decision

We will decide a series' identity once, at the series level, from reconciled
evidence, and make every episode inherit it:

1. **Stated show year per file**: file name `Show (YYYY)` prefix (when the name
   part matches the folder name, fold + similarity ≥ 0.8) ▸ folder `(YYYY)` ▸
   release folder; a filename year that differs from the folder's year is
   conflicting evidence, not overruled by the folder, and a folder takes its
   files' consensus when they agree. `tvshow.nfo`'s year and ids are evidence,
   never identity (V890).
2. **Split a folder by stated year** when its files state different years,
   whether the folder is bare or yeared. A file with no year splits off only
   when its own episode titles fit a same-named rival better; when the server
   decides that from episode-title evidence, it records a `series_overrides` row
   with `source = 'scanner'` for that path so later scans apply it. The group
   holding the folder's stored match (whose stated year is within one year of
   the stored match's premiere year, else the group with the most episodes)
   keeps the plain folder key, so the show users know keeps its wire id and
   series-level state; that choice is persisted as a `scanner` override at the
   first split so later scans never move it. Split keys are
   `"{folder}\u{1f}{YYYY}"`, stored in a new `media_items.series_key` column
   backfilled to today's key (V893).
3. **No silent tie-break**: at most 3 exact-name candidates without a separating
   year have their episode lists scored by episode fit (season/episode coverage,
   filename titles, and the NFO ids and year as one vote: an NFO year never
   separates candidates). The best one is used, and when it does not lead by a
   clear margin, or when more exact-name candidates exist than were scored, the
   match is flagged `'ambiguous'` for an admin to check. Manual matches are
   never re-asked (V891).
4. **Episodes take metadata only from their series' pinned match**; the
   per-episode series search goes, and an episode NFO id naming another series
   is set aside with its text and stills (V892, extends V468).
5. **Copies never fold across series**: #1055's fold also requires one series
   key, no two different known stated years (two copies whose filename-stated
   years differ never fold, whatever the folder year says), equal provider
   series ids where both are known, and agreeing filename episode titles where
   both have one. Rows placed by an admin override never fold unless the admin
   explicitly picks "merge as versions" for that pair. In a flagged or split
   series, numbers alone never fold two rows (V894).
6. **Hand fixes are overrides**: an admin Show identity page picks a show,
   confirms a flagged guess (stored as a manual match), moves files between
   shows or undoes a split; each fix is stored in `series_overrides` (migration
   0074, shipped with the scanner change) and applied by the scanner after its
   split and before the copy fold, so a rescan never undoes it. `scanner` rows
   record the folder-key owner and per-file title-evidence moves; an `admin`
   row always wins over a `scanner` row, and rows placed by an admin override
   are marked so the copy fold keeps both rows and their user data unless
   explicitly merged (V896).
7. **Repair by dry run, then apply**: `[metadata] series_identity_repair`
   defaults to `"dry_run"`, which also gates the split itself (logging the
   groups and keys a split would write without writing the new `series_key`
   values or the owner row). `"apply"` writes them, re-matches only shows with
   conflicting evidence (the version gate no longer re-admits every searched
   show), and on every automatic change of a show's stored match durably marks
   that show's episodes for reconciliation with a stored pending marker.
   Reconciliation rebuilds pending episodes (and episodes whose provider show
   disagrees with their series or whose premiere date falls outside its run)
   against the newly stored match, retrying on later passes and clearing the
   marker only when complete. The rebuild resets provider-derived fields
   (enrichment only fills empty ones) and deletes the episode's stored
   translations. An episode's own manual match stays exempt; a conflict is
   listed for the admin. No row is deleted.

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
  yeared show, and every split is logged and listed on Manage → Show identity.
- Follow-up: the split 2015 group gets a new series wire id; series-level
  favourites stay with the unyeared group.

## Alternatives considered

- **Trust `tvshow.nfo` ids first.** Wrong on most of the confused folders'
  evidence; would fix the bare-folder cases and break the NFO-id cases.
- **Trust the search, add the filename year only.** Fixes the bare-folder cases;
  leaves the episode-NFO-id cases and B933 (per-episode searches) broken.
- **Prefer the newer show on a tie.** Swaps which half of the pairs is wrong.
- **Never split; only flag mixed folders.** Simpler and touches no wire ids, but
  leaves a mixed folder's 2015 episodes under one 1991 match until the files are
  moved. Alison chose the split with hand remapping (2026-11-09).
- **Leave an undecided show unmatched until picked.** Alison chose a flagged
  best guess instead (2026-11-09).

## References

- Spec 033 (`specs/033-series-identity/spec.md`).
- LIB-C11 (`pharos-core` `SeriesInfo::series_key`), B557 (`match_best`), B558 /
  V468 (foreign NFOs), #1055 / ADR 0079 (episode copies as versions).
