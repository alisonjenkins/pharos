# Spec 033 — Telling apart same-named series

- **Status:** Reviewed by Alison 2026-11-09T17:27Z (open questions answered, §8). No code yet.
- **Date:** 2026-11-09T14:30:00Z
- **ADR:** [0086](../../docs/adr/0086-series-identity-is-decided-from-reconciled-evidence.md)
- **Claimed ids:** B930–B934, V890–V895 (entries go into
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

1. the show folder's `(YYYY)` (today's rule);
2. a `Show Name (YYYY)` prefix in the file name, before its `SxxEyy` token,
   whose name part matches the folder name (fold + similarity ≥ 0.8, as B558);
3. the release folder's year (today's rule).

`series_year` becomes the folder's **consensus** of its files' stated years when
the folder name has none: BSG gets 2003 from 81 of 81 files. `tvshow.nfo`'s year
is used only when no file states one and the NFO passes the B558 name check, and
then only as matching evidence (§3), never as identity.

### 2. Splitting a folder that holds two shows (V893)

When a bare folder's files state two or more different years, or some state a
year and others state none, the folder is **split by stated year**:

- the files with no stated year keep the plain folder key, so its wire ids and
  series-level user data (favourite, played) do not move;
- each stated-year group gets the key `"{folder}\u{1f}{YYYY}"` (unit separator,
  which never occurs in a path) and `series_year = YYYY`;
- an unyeared file whose name carries an episode title that matches an episode
  of one yeared group's matched show (same season/episode, similarity ≥ 0.8)
  joins that group instead. This keeps a stray `S03E05 - Title.mkv` with the
  2014 show without a rename.

A folder whose files all state one year (BSG) is not split. For the deployed
library this splits exactly one folder, `The Flash`: 4 files stay on the folder
key (1990), 62 move to `…\u{1f}2014`. The UI shows two "The Flash" tiles,
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

1. A year (folder, consensus, or trusted NFO) goes into the search as today.
2. With no year, if two or more candidates match the name exactly, the matcher
   no longer tie-breaks by provider order. It fetches each exact candidate's
   season/episode list (at most 3 candidates, cached) and scores them:
   - share of local (season, episode) pairs the candidate has;
   - share of filename episode titles that match the candidate's titles at the
     same position;
   - a vote, not a veto, for the candidate the NFO ids name.
3. The best candidate always wins, so the show shows its best guess. When it
   leads the runner-up by less than a clear margin (proposed 0.2 on a 0–1
   scale), the match is also **flagged**: `series_metadata.match_flag =
   'ambiguous'` with the runner-up's provider id and both scores, logged. A
   flagged show carries a "Check match" badge for admins on its title page and
   is listed on the Show identity page (§7) until it is fixed or confirmed.
   A manual match (`match_source = 'manual'`) is never re-asked.

Bump `TV_MATCH_VERSION` to 2 so stored matches are re-asked; see §6 for how the
re-ask is limited.

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
(after the §2 split) **and** their stated show years are equal (both absent
counts as equal) **and**, when both rows carry a provider series id, those ids
are equal. The split runs before the fold in a scan pass, so a split folder's
groups are separate shows before #1055 looks at them. A refused fold is logged
with both paths and the reason and counted under its own `decided_by` label.

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
   `match_runner_up_score` for §3's flag.
2. **Dry run first**: `[metadata] series_identity_repair = "dry_run"` (default)
   logs, per series, what the next pass would change (split groups, old and new
   match, episodes whose metadata would be re-fetched) and counts it, without
   writing. `"apply"` makes the writes. The live switch to `"apply"` is a
   home-cluster PR that needs Alison's OK, like the artwork enforcement.
3. **Re-match only shows with conflicting evidence**: with `TV_MATCH_VERSION`
   at 2, a stored search match is re-asked only when its show has an exact-name
   rival, or its matched premiere year differs from the stated year by more
   than one, or the folder was split. Manual matches are kept. In the deployed
   library this re-asks BSG and The Flash (and checks the other bare-folder
   remakes, which keep their match).
4. **Re-fetch episodes whose show id disagrees**: an episode whose stored
   provider ids belong to a show other than its series' pinned match, or whose
   premiere date falls outside the pinned show's run, is cleared
   (`metadata_refreshed_at = NULL`) and re-enriched by §4. In the deployed
   library that is about 176 rows (BSG 81, Flash 66, Cosmos 13, Life on Mars
   16) plus the 6 B933 rows.
5. **Series-level state of split rows**: the 62 Flash 2014 rows get a new series
   wire id. A favourite or played mark set on "The Flash" series stays with the
   folder key (the 1990 group), since the group that keeps the key is the one
   with no stated year. Episode-level state does not move.

### 7. Fixing shows and files by hand: the Show identity page (V895)

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
   or confirm the guess, which clears the flag.
2. **Move files** — tick episodes of a folder and send them to another group:
   one of the folder's groups, any other show in the library, or a new show
   (name, year, and optionally a provider pick). This is how a wrong split is
   undone or a missed one is made.
3. **Keep together** — undo a split for the whole folder.

Hand fixes are stored as **overrides**, never by editing the scanned rows, so
a rescan cannot undo them: a new table `series_overrides` (path or folder,
target series key, target name and year, who, when; migration 0074, verify
free). The scanner applies overrides after its own split and before #1055's
fold, so the fold sees the final grouping. Each change re-fetches the affected
episodes' metadata straight away (§4) and keeps every row and its user data.
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
  `outcome` ∈ `matched | ambiguous | no_match | kept_manual`, `decided_by` ∈
  `year | episode_fit | nfo_vote | sole_candidate`.
- `{namespace="pharos"} |= "series match"` with `query=… year=… candidates=[…]
  chosen=… runner_up=… margin=…` on every decision, matched or not.
- `sum by (reason) (pharos_series_folder_split_total)`.
- `sum by (reason) (increase(pharos_episode_metadata_rejected_total[1d]))`,
  `reason` ∈ `nfo_other_series | outside_run | title_mismatch`, log carrying
  path, NFO id, pinned id.
- `sum by (decided_by) (pharos_scan_copy_primaries)` gains
  `decided_by="refused_other_series"` for V894.
- `{namespace="pharos"} |= "series identity repair"` for each dry-run line.
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
| V890 | A show's year comes from folder ▸ filename `Show (YYYY)` prefix ▸ release folder, per file, with the folder taking its files' consensus; `tvshow.nfo` year is evidence only. |
| V891 | Two exact-name candidates with no year between them are decided by episode fit, or stored as `ambiguous` for a manual pick; never by provider order. |
| V892 | An episode's provider metadata comes only from its series' pinned match; an episode NFO id naming another series is set aside. |
| V893 | A bare folder whose files state different show years is split by stated year; the unyeared group keeps the folder key. |
| V894 | Copies fold only within one series key, with equal stated show years and equal provider series ids where both are known. |
| V895 | A hand fix of a show or its files is an override the scanner applies after its own split and before the copy fold; a rescan never undoes it. |

## Out of scope

- Moving files on disk. The NAS is the user's; the server never renames.
- Films: already keyed by name with year (#1055, `duplicates.rs`).
- Music: same-named artists are a separate problem (MusicBrainz ids).
