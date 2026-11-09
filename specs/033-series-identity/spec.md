# Spec 033 — Telling apart same-named series

- **Status:** Reviewed by Alison 2026-11-09T17:27Z (open questions answered, §8). No code yet.
- **Date:** 2026-11-09T14:30:00Z
- **ADR:** [0086](../../docs/adr/0086-series-identity-is-decided-from-reconciled-evidence.md)
- **Claimed ids:** B930–B934, V890–V896 (entries go into
  `001-pharos-baseline/bugs.md` / `invariants.md` with the fix, not before).

## Problem

Alison (2026-11-09): shows that were remade under the same name (Cosmos 1980 and
2014, Battlestar Galactica 1978 and 2004, The Flash 1990 and 2014) get confused
by the scanner. The episodes show the other show's titles, plots and dates.

Series identity is already keyed on the show folder (LIB-C11,
`SeriesInfo::series_key`), so two folders never merge their episodes. The
confusion is in **which provider show a folder is matched to**, and in **which
provider episode each file takes its metadata from**.

## What the live library shows (read-only queries, 2026-11-09T14:09Z)

Remake pairs present: Battlestar Galactica, Cosmos, The Flash, Charmed, Knight
Rider, Quantum Leap, One Piece, Life on Mars. In every pair the remake sits in a
`Show (YYYY)` folder and the other in a bare `Show` folder. All year folders
match correctly (confidence 0.95–1.0), and so do both sides of Charmed, Knight
Rider, Quantum Leap and One Piece. Only bare folders go wrong. **176 episode
rows show another show's metadata.** No row has a NULL `series_folder`.

| Folder | Episodes | On disk | Series match | Episode metadata | Failure |
|---|---|---|---|---|---|
| `Battlestar Galactica` | 81 | every file `Battlestar Galactica (2003) - SxxEyy`; `tvshow.nfo` says 2003 | 1978 (tvdb 71173, conf 0.85) | 1978 titles ("33" → "Saga of a Star World (1)") | B930 |
| `The Flash` | 66 | Season 01: 4 files of the 1990 series, no year; Seasons 03–09: 62 files `The Flash (2014) - SxxEyy` | 1990 (tvdb 78650) | from Jellyfin NFOs (2014), so the 1990 files carry 2014 titles | B930, B932 |
| `Cosmos` | 13 | the 1980 Sagan series; Jellyfin `tvshow.nfo` and episode NFOs say 2014 | 1980 (tvdb 74995), correct | 2014 titles, plots, air dates from the NFO ids | B931 |
| `Life on Mars` | 16 | UK 2006 (8 + 8, `Episode N` naming; inferred from count and naming) | UK, correct | US 2008 from the NFO ids | B931 |

Also found: the per-episode search attached unrelated shows' episodes in
`Dune - Prophecy` S01E06 ("First Date", 2007), `Knight Rider (2008)` S01E02/E04
("Condom Mania", 2003) and `Star Trek - Starfleet Academy` S01E02/E04/E05
(French crime-show titles) — B933.

The NFOs are wrong as often as the search: `tvshow.nfo` loses on Cosmos, Life on
Mars, Dragon Ball (B558) and Forever (US) and is right on The Flash and BSG. So
"trust the NFO" does not fix this, and neither does "trust the search".

## Causes in the code

1. **The filename's show year is ignored (B930).** `parse_series_info`
   (`pharos-scanner/src/fs.rs`) takes `series_year` from the folder name, then
   a release folder. `Battlestar Galactica (2003) - S01E01.mkv` in a bare folder
   gives `series_year = None`. `match_best` then scores both "Battlestar
   Galactica" candidates as exact names at the no-year factor 0.85, and the
   tie-break ("an earlier name wins", then the earliest candidate) picks the
   1978 show. The `tvshow.nfo` year is not used either.
2. **An ambiguous exact name is matched silently (B930).** When two candidates
   carry exactly the query name and no year separates them, `match_best`
   returns one at 0.85, over the 0.7 floor, with nothing logged about the
   rival.
3. **Episodes search on their own (B931, B933).** `enrich_one` searches the
   provider for every episode by series name and year
   (`metadata_backfill.rs`, `MediaKind::Episode` arm), or takes the episode
   NFO's id as authoritative (`resolve`, `nfo_id`). Neither checks that the
   episode belongs to the show the folder was matched to in `series_metadata`.
   That is how Cosmos 1980 files take 2014 episodes, and how Dune - Prophecy
   takes a 2007 sitcom's episode.
4. **One folder can hold two shows (B932).** The key is the folder, so the
   1990 and 2014 Flash files are one series and one match.

## Design

### 1. Show year per file (V890)

The scanner derives a **stated show year** for each episode file, first hit wins:

1. a `Show Name (YYYY)` prefix in the file name, before its `SxxEyy` token,
   whose name part matches the folder name (fold + similarity ≥ 0.8, as B558);
2. the show folder's `(YYYY)` (today's rule);
3. the release folder's year (today's rule).

A filename that states a show year different from the folder's own year (for
example `The Flash (2014) - S01E01.mkv` inside a folder `The Flash (1990)`) is
conflicting evidence, not overruled by the folder. `series_year` becomes the
folder's **consensus** of its files' stated years when all its files agree on
one year: BSG gets 2003 from 81 of 81 files. `tvshow.nfo`'s year is used only
when no file states one and the NFO passes the B558 name check, and then only as
matching evidence (§3), never as identity.

### 2. Splitting a folder that holds two shows (V893)

A folder (bare or yeared) is **split by stated year** when its files state two
or more different years. Files that state no year are not split off for that
alone: a folder of `Show (2020) - S01E01.mkv` and `S01E02.mkv` is one show. An
unyeared file joins the folder's yeared group unless its own evidence fits
another same-named show better: its filename episode title matches that show's
episode at the same season/episode (similarity ≥ 0.8) and does not match the
yeared show's. When the server (not the scanner) decides from episode-title
evidence that an unyeared file belongs to a same-named rival show, it records
that as a `series_overrides` row with `source = 'scanner'` for that one path
(§7), so the scanner applies it on later passes like any override. An unyeared
file with no title to compare stays with the yeared group, and the folder is
flagged (§7) when the same-named rival exists.

Which group keeps the plain folder key, and with it the series wire id and the
series-level user data (favourite, played):

1. the group whose show is the folder's stored `series_metadata` match, decided
   by year: the group whose stated year is within one year of the stored
   match's premiere year, so the show users already know keeps its id;
2. else, if none fits, the group with the most episodes.

The choice is made once, at the folder's first split, and persisted as a
scanner-written row in `series_overrides` (§7, `source = 'scanner'`). Later
scans reuse it even if another group grows larger or the match goes away, so
series state never hops between shows. A hand override (`source = 'admin'`)
replaces it.

Every other group gets the key `"{folder}\u{1f}{YYYY}"` (unit separator, which
never occurs in a path), or `"{folder}\u{1f}none"` for an unyeared group that
does not keep the plain key, with `series_year` set to its year.

A folder whose files all state one year (BSG) is not split, so it keeps its key
and its state. For the deployed
library this splits `The Flash`: its 4 Season 01 files carry
1990 episode titles, so they split from the 62 `(2014)` files. The stored match
is the 1990 show, so the 4 files keep the folder key and the 62 move to
`…\u{1f}2014`. The UI shows two "The Flash" tiles,
told apart by `ProductionYear` as LIB-C11 already does for two folders.

Every split is logged with the folder, the groups, their counts and the reason
(`series folder split folder=… groups=[none:4, 2014:62] reason=stated_years`),
and counted. Every split folder is listed on the Show identity page (§7), where
a wrong split is fixed by hand.

### 3. Matching a show (V891)

`series_needing_match` hands the matcher the evidence, not just name and year:
the stated or consensus year, the local season list with episode counts, up to
ten filename episode titles, and the `tvshow.nfo` provider ids and year (when
B558 trusts it).

1. A year from the folder or the files' consensus goes into the search as
   today and separates same-named candidates. A year that comes only from
   `tvshow.nfo` does not separate them: it is one more vote in step 2, since
   the NFOs in this library are wrong as often as right.
2. If two or more candidates match the name exactly and no folder or file year
   separates them, the matcher no longer tie-breaks by provider order. At most 3
   exact-name candidates have their season/episode lists fetched (cached) and
   scored:
   - share of local (season, episode) pairs the candidate has;
   - share of filename episode titles that match the candidate's titles at the
     same position;
   - a vote, not a veto, for the candidate the NFO ids or NFO year name.
3. The best candidate always wins, so the show shows its best guess. When it
   leads the runner-up by less than a clear margin (proposed 0.2 on a 0–1
   scale), or when more exact-name candidates exist than were scored, the match
   is also **flagged**: `series_metadata.match_flag = 'ambiguous'` with the
   runner-up's provider id and both scores, logged. A flagged show carries a
   "Check match" badge for admins on its title page and is listed on the Show
   identity page (§7) until it is fixed or confirmed. A manual match
   (`match_source = 'manual'`) is never re-asked.

Bump `TV_MATCH_VERSION` to 2 so stored matches are re-asked, but only after
the version gate is changed (§6 step 3): today `readmit_stale_tv_matches`
re-admits every searched show and episode on any version change.

### 4. Episodes inherit the show's match (V892)

An episode's provider metadata comes **only** from its series' pinned match in
`series_metadata` (`match_provider`, `match_external_id`):

- the per-episode series search in `enrich_one` goes away; an episode with no
  series match yet waits for it (as a movie waits for a provider key today);
- an episode NFO id is used only if the provider says that episode belongs to
  the pinned series id. Otherwise the NFO's id, text and stills are set aside,
  extending B558/V468 from show names to ids;
- the episode is addressed in the pinned show by season/episode (absolute
  number for anime, as now), with the filename title as a check: a title that
  matches nothing in the pinned show, where the season/episode lookup gives a
  different title, is logged as a mismatch.

This fixes Cosmos and Life on Mars (NFO ids from the remake) and B933 (searches
landing in unrelated shows) at one root, and makes an episode's metadata change
together with its show's when the show is re-matched.

### 5. Versions and dedup (#1055) must never fold across shows (V894)

#1055 folds copies that share one show folder, season and episode. With this
design the rule becomes: copies fold only when they share the **series key**
(after the §2 split) **and** their stated show years do not conflict (two
different known years conflict, and two copies whose filename-stated years
differ never fold, whatever the folder year says; one known and one absent do
not conflict, since §2 keeps an unyeared file with the yeared group) **and**,
when both rows carry a provider series id, those ids are equal **and** the
evidence does not say they are different episodes: when both file names carry an
episode title, the titles match (similarity ≥ 0.8). Rows placed by an admin
override (§7) never fold unless the admin explicitly picks "merge as versions"
for that pair. Matching season/episode numbers alone fold only in a series that
is neither flagged (§3) nor split (§2); in a flagged or split series, copies
with no provider id or title to compare are kept as separate rows and listed on
the Show identity page (§7) instead. The split runs before the fold in a scan
pass, so a split folder's groups are separate shows before #1055 looks at them.
A refused fold is logged with both paths and the reason and counted under its
own `decided_by` label.

Today's library is safe for #1055 as it stands: the only mixed folder (The Flash)
has no season in both shows (1990 is Season 01, 2014 is Seasons 03–09), so no
copy pair crosses shows. The guard is for the next mixed folder.

For films, #1055 already keys a copy by its name including the year, so
`Dune (1984)` and `Dune (2021)` never fold, and `duplicates.rs` already keeps
same-title-and-year rows with different provider ids apart.

### 6. Migration for rows already wrong

No row is deleted. Item ids, play state, resume and favourites stay on their
rows. What changes is provider text, artwork and, for the 62 split Flash rows,
the series wire id.

1. **Schema**: new nullable `media_items.series_key` (migration 0073, verify
   free at implementation), backfilled as `COALESCE(series_folder,
   series_name)`, which is exactly today's key, so no wire id changes on
   deploy. The scanner writes the split key on the next pass. Queries that
   pivot on `series_folder` move to `series_key`; `series_metadata.series_key`
   is already the same value. The same migration adds nullable
   `series_metadata.match_flag`, `match_runner_up_id` and
   `match_runner_up_score` for §3's flag. The `series_overrides` table
   (migration 0074, §7) ships with the scanner change, because the folder-key
   owner row is written at the first split.
2. **Dry run first**: `[metadata] series_identity_repair = "dry_run"` (default)
   logs, per series, what the next pass would change (split groups, old and new
   match, episodes whose metadata would be re-fetched) and counts it, without
   writing. The gate also covers the split itself: in `"dry_run"` the scanner
   logs the groups and keys a split would write (`series identity repair` lines)
   and writes neither the new `series_key` values nor the owner row; only
   `"apply"` writes them, so no series wire id moves before the switch. The live
   switch to `"apply"` is a home-cluster PR that needs Alison's OK, like the
   artwork enforcement.
3. **Re-match only shows with conflicting evidence**: `readmit_stale_tv_matches`
   no longer calls `readmit_searched_tv` (which re-admits every searched show
   and episode) on a version change. At version 2 it runs the conflict filter
   below, re-admits only what it selects, and only in `"apply"`; in
   `"dry_run"` it logs the selection and leaves the stored version key at 1, so
   switching to `"apply"` later still runs it. A stored search match is
   re-asked only when its show has an exact-name
   rival, or its matched premiere year differs from the stated year by more
   than one, or the folder was split. Every automatic change of a show's stored
   match durably marks that show's episodes for reconciliation with a stored
   pending marker. Manual matches are kept. In the deployed
   library this re-asks BSG and The Flash (and checks the other bare-folder
   remakes, which keep their match).
4. **Re-fetch episodes whose show id disagrees**: an episode marked pending
   reconciliation (step 3), or whose stored provider ids belong to a show other
   than its series' pinned match, or whose premiere date falls outside the
   pinned show's run (whether or not its provider show can be identified), is
   **rebuilt**, not just re-admitted: `apply_enrichment` only fills empty
   fields, so clearing `metadata_refreshed_at` alone would keep the wrong title,
   plot and air date. Reconciliation runs against the newly stored match and is
   retried on later passes until it completes; the pending marker clears only
   when it has. The rebuild re-runs the local resolver (NFO ▸ sidecar ▸
   filename) with the rejected NFO set aside, which resets the provider-derived
   fields (title, overview, premiere date, community rating, provider ids, the
   episode still) to what local evidence alone says, then enriches from the
   pinned match. Local data that passes the checks (a sidecar, an NFO that names
   the right show) is re-read and so kept. The episode's rows in the
   translations table (`MetadataTranslationStore`) are deleted in the same step,
   so no language's text from the old show survives a fetch that omits that
   language. Probe data, user data and the row id do not change.

   An episode with its own manual match (`match_source = 'manual'`) stays exempt
   from §4's pinned-show rule and from reconciliation: repair never rebuilds it.
   A conflicting manual match is kept and listed on the Show identity page (§7)
   with both shows, for the admin to keep or move. In the deployed library that
   is about 176 rows (BSG 81, Flash 66, Cosmos 13, Life on Mars 16) plus the 6
   B933 rows.
5. **Series-level state of split rows**: the 62 Flash 2014 rows get a new series
   wire id. A favourite or played mark set on "The Flash" series stays with the
   folder key, which the 1990 group keeps because it holds the stored match
   (§2). Episode-level state does not move. A folder that is not split keeps
   its key and state.

### 7. Fixing shows and files by hand: the Show identity page (V896)

Alison (2026-11-09): "we need to have a way to remap them somehow and fix them."
A new admin page, Manage → **Show identity**, lists every show that needs a
look, newest first, with the reason in words:

- a folder the scanner split, with each group's year and file count;
- a flagged match (§3), with the guess and the runner-up side by side (name,
  year, poster, episode count);
- episodes whose provider data was rejected (§4), with the reason and value.

Each entry offers:

1. **Pick the show** — search the providers and pick a match (the existing
   Identify path, `RemoteSearch/Apply`, which writes `match_source = 'manual'`),
   or confirm the guess, which clears the flag and also writes
   `match_source = 'manual'`, so `series_needing_match` never re-admits it
   after the refresh TTL.
2. **Move files** — tick episodes of a folder and send them to another group:
   one of the folder's groups, any other show in the library, or a new show
   (name, year, and optionally a provider pick). This is how a wrong split is
   undone or a missed one is made.
3. **Keep together** — undo a split for the whole folder.

Hand fixes are stored as **overrides**, never by editing the scanned rows, so
a rescan cannot undo them: a new table `series_overrides` (path or folder,
target series key, target name and year, `source` = `admin` or `scanner`,
who, when; migration 0074, verify free). `scanner` rows record §2's folder-key
owner and per-file title-evidence moves; an `admin` row always wins over a
`scanner` row. The scanner applies overrides after its own split and before
#1055's fold, so the fold sees the final grouping. Rows placed by an admin
override are marked as such; the copy fold (#1055) never merges a row an admin
override placed, so moving an episode into a show that already has that
season/episode keeps both rows and their user data across rescans, unless the
admin explicitly picks "merge as versions" for that pair. Each change rebuilds
the affected episodes' metadata straight away (§6 step 4) and keeps every row
and its user data. A moved episode's own manual match (`match_source = 'manual'`,
which `items_needing_match` skips) is cleared by the move and the episode is
rebuilt directly against the destination show, outside the queue, so no metadata
from the old show survives. The move is the admin's resolution; the log line
names the cleared manual id.
Overrides are listed on the page and can be removed, which returns the files to
what the scanner decides.

Signal: `sum by (action) (increase(pharos_series_override_total[1d]))`,
`action` ∈ `pick_show | confirm | move_files | keep_together | remove`, and a
log line naming the admin, paths and old and new series keys.

### 8. Decisions (Alison, 2026-11-09T17:27Z)

1. Split mixed folders automatically, with a way to remap and fix them (§2, §7).
2. Yes, the server may re-match BSG and The Flash and re-fetch metadata on about
   180 episode rows once `"apply"` is set. Nothing is deleted.
3. A show the matcher can't decide between shows its best guess with a flag,
   and can be fixed (§3, §7).

## Signals (ODD)

The bug today, by query (no metric yet, so the first implementation commit adds
these):

- `sum by (outcome, decided_by) (increase(pharos_series_match_total[1d]))`,
  one count per show per pass. `outcome` ∈ `matched | ambiguous | no_match |
  transient | kept_manual`, `decided_by` ∈ `sole_candidate | name | year |
  earliest_premiere | provider_order | episode_fit | nfo_vote | none`.
  `name`, `earliest_premiere` and `provider_order` are the rules the matcher
  uses before this spec; `provider_order` is B930 itself, so it is counted
  under its own label until §3 removes it. `transient` is a chosen match whose
  record did not arrive. `none` goes with `no_match`.
- `{namespace="pharos"} |= "series match"` with `query=… year=… candidates=[…]
  chosen=… runner_up=… margin=…` on every decision, matched or not.
- `sum by (reason) (pharos_series_folder_split_total)`: a gauge each complete
  scan pass of a root sets to the folders it splits under that root (labels
  `reason` = `stated_years`, `root`), in both modes.
- `sum by (change, mode) (pharos_series_identity_repair_rows)`, a gauge per
  `root`, `change` ∈ `series_key | owner_row`, `mode` ∈ `dry_run | apply`: the
  rows the root's last complete pass found to differ. In `apply` they were
  written, so a settled library reads 0; in `dry_run` they stay pending and
  read the same each pass.
- `sum by (reason) (increase(pharos_episode_metadata_rejected_total[1d]))`,
  `reason` ∈ `nfo_other_series | outside_run | title_mismatch`, log carrying
  path, NFO id, the series the record was read from and the pinned id.
  `nfo_other_series` records are set aside (§4); `title_mismatch` is
  observation only. The first instrumentation also counted
  `search_other_series` (B933); §4 removes the per-episode search, so that
  label is gone.
- `sum by (decided_by) (pharos_scan_copy_primaries)` gains
  `decided_by="refused_other_series"` for V894.
- `{namespace="pharos"} |= "series identity repair"`: one info line per folder
  a pass would change (or changed), with the row count; the per-row lines
  (`series identity repair: row`) are debug.
- `sum by (action) (increase(pharos_series_override_total[1d]))` for §7.

After deploy, success is: the dry-run lines list BSG → 2004 (tvdb id of the
2004 series), The Flash split 4 + 62, Cosmos 13 and Life on Mars 16 episode
re-fetches, and nothing else changing match.

## Ids

| Id | What |
|---|---|
| B930 | A bare show folder whose files state the show year (`Show (2003) - S01E01`) matched the older same-named show: the filename year and `tvshow.nfo` year were ignored, and an ambiguous exact name tie-broke silently (BSG, The Flash). |
| B931 | Episode NFO ids from a same-named remake were taken as authoritative although the folder was matched to the other show (Cosmos, Life on Mars). |
| B932 | One show folder holding two shows (The Flash 1990 Season 01 and 2014 Seasons 03–09) was one series with one match. |
| B933 | The per-episode series search attached unrelated shows' episodes (Dune - Prophecy S01E06, Knight Rider (2008) S01E02/E04, Star Trek - Starfleet Academy S01E02/E04/E05). |
| B934 | Reserved for an issue found while implementing. |
| V890 | A show's year comes from filename `Show (YYYY)` prefix (when the name matches the folder) ▸ folder `(YYYY)` ▸ release folder, per file, with the folder taking its files' consensus; a `tvshow.nfo` year is a vote only and never separates same-named candidates. |
| V891 | At most 3 exact-name candidates with no folder or file year between them have their episode lists scored, never tie-breaking by provider order; the best guess is used and a close call or more exact-name candidates than were scored is flagged for an admin. |
| V892 | An episode's provider metadata comes only from its series' pinned match; an episode NFO id naming another series is set aside. |
| V893 | Any folder whose files state different show years is split by stated year; an unyeared file splits off only on its own episode-title evidence (recorded as a `scanner` override); the group within one year of the stored match's premiere year (else the largest) keeps the folder key. |
| V894 | Copies fold only within one series key, with no two different known stated years (and never when filename-stated years differ, whatever the folder year says), equal provider series ids where both are known, agreeing filename episode titles where both have one, and neither row placed by an admin override unless explicitly merged; in a flagged or split series, numbers alone never fold. |
| V896 | A hand fix of a show or its files is an override the scanner applies after its own split and before the copy fold; a rescan never undoes it, and the copy fold never merges a row an admin override placed unless the admin explicitly merges that pair. |

## Out of scope

- Moving files on disk. The NAS is the user's; the server never renames.
- Films: already keyed by name with year (#1055, `duplicates.rs`).
- Music: same-named artists are a separate problem (MusicBrainz ids).
