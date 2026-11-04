# 024 — Dynamic music mixes

- **Status:** Slice 1 (engine + native API) in progress; slice 2 (UI) next.
- **Date:** 2026-11-04
- **Asked by:** Alison, "an Amarok-style auto playlist generator (or better)".
- **Ids:** B440–B444, V380–V384 (taken: B440, B441, V380–V382). ADR-0048.
  Schema v9 (main+1 when it was rebased; renumber again if another
  schema change lands first).

## What Amarok does, and what we keep

Amarok's dynamic playlists keep the queue topped up from a set of *biases*:
"match" biases that a share of tracks should meet ("50% from the 80s"),
tag-match filters, "if/else" combinations, and Last.fm "similar artists".
It solves them with a constraint search over the whole collection, and the
listener can't see why a track was picked.

Pharos keeps the core idea (rules plus a share, a queue that refills itself)
and adds:

- **Strengths in words**: each rule is *Only*, *More*, *Less* or *Never*,
  rather than a percentage on everything. A share ("about 30% of the songs")
  is an option on a *More* rule.
- **Reasons**: every song the mix picks carries the rules it matched, shown
  in the queue ("Trip-hop · Not played for 90 days").
- **No silent bending**: *Only*/*Never* rules never bend. When a mix runs
  out it says so; when spacing or a share has to give way, the batch names
  it and the server counts it.
- **Radio and drift**: start a mix from any song ("more like this"), or let
  it follow the song that just played so it wanders through the library.
- **Spacing**: no artist again within N songs, no album within M.
- **Presets**: Drift, Favourites, Rediscover, Deep cuts, New arrivals; save
  your own.

## Rule model

A **mix** is a list of rules (at most 32), a seed, and two spacing gaps.

| Test | Matches a song when | Data it reads |
|---|---|---|
| Genre *g* | one of its genres is *g* (case-insensitive) | `probe.genre`, split like the genre entity |
| Artist *a* | its track or album artist is *a* | `probe.artist`, `probe.album_artist` |
| Album *a* | its album is *a* | `probe.album` |
| Years *from–to* | its release year is in range | `probe.year`, else `metadata.production_year` |
| Favourites | the user starred it | `user_data.is_favorite` |
| Never played | play count 0 and never started | `user_data` |
| Play count *min–max* | played that many times | `user_data.play_count` |
| Not played for *n* days | last played ≥ *n* days ago, or never | `user_data.last_played_at` |
| Played in the last *n* days | last played < *n* days ago | `user_data.last_played_at` |
| Added in the last *n* days | added < *n* days ago | `media_items.created_at` |
| Length *min–max* | its length in seconds is in range | `probe.duration_ms` |
| Like the seed | same artist, or close in genre, era, tempo and energy (likeness ≥ 0.35) | the seed song |
| Tempo *min–max* BPM | its tempo is in range | `probe.bpm`: the file's BPM tag, else the scanner's analysis (B445) |
| Energy *min–max* % | its energy is in range (0 calm, 100 intense) | `probe.energy`, from the scanner's analysis (B445) |

**Likeness** (0–1): the same artist is 1; otherwise a weighted mean of
the genre sets' Jaccard (0.5), closeness in year, 1 − |gap| / 20 (0.2),
closeness in tempo, counting half and double time as the same beat (0.15),
and closeness in energy (0.15). Tempo and energy count only when the seed
has them; a song not yet analysed scores halfway on them. So an unanalysed
library scores as genre 0.7 / era 0.3. A *More* like-seed rule weights by
likeness, not just yes/no. Tempo and energy come from the "tempo and
energy" scanner work (B445, `MediaProbe.bpm` / `.energy`); until a track
is analysed they are unknown and the tempo and energy rules skip it. A
similar-artists source is still a follow-up.

**Strengths**: *Only* (`require`) and *Never* (`exclude`) decide what is
eligible. *More* (`prefer`) multiplies a song's chance by 4, *Less*
(`avoid`) divides it by 4. A *More* rule with a share instead aims for
that percentage of songs.

**Seed**: none, a song (radio), or *follow*: the last song heard or
queued, then each pick in turn, so the mix walks from song to song
("Drift"). Hard like-seed rules hold against the song the batch started
from; the soft ones follow the walk.

## How a batch is picked

The server engine (`services::mix::select`, pure) runs per pick:

1. Eligible = songs the user may hear (library access and parental rating,
   as `Library.query`) that pass every *Only*/*Never* rule.
2. Freshness: songs in `recent` or already picked are out while a fresh
   one is left. Otherwise the older half of `recent` may repeat
   (`relaxed: repeat`).
3. Spacing: drop songs whose artist was in the last `artistGap` songs or
   whose album was in the last `albumGap`; if that leaves nothing, ignore
   spacing for this pick (`relaxed: spacing`).
4. Shares: for each share rule, look at the last 20 songs; with target *p*
   and observed *q*, the next song must match with probability
   clamp(2*p* − *q*). If nothing satisfies that, the share sits out this
   pick (`relaxed: share`). Shares bend before spacing: a share corrects
   itself over the next picks, a repeated artist cannot be taken back.
5. Weighted random draw over what's left.

If eligible songs run out before `count`, the batch comes back short with
`exhausted`.

## How the mix feeds the queue

The call is **stateless**: `Library.mix(spec, recent, count)`. The client
owns the mix: it keeps the spec and sends what was heard or queued
(`recent`, oldest first, the server reads at most 200), so a reload,
another device or a different client can carry on with no server session.

The play queue (`pharos-ui` `native/queue.rs`, built by the play-queue
work) holds entries tagged with a `Source`. Mix entries are `Source::Mix`.

- **Start**: picking a mix (or "Mix from this song") drops queued mix
  entries, asks for 10 songs, and plays the first.
- **Top up**: when a mix is on and fewer than 5 upcoming entries are left,
  the UI asks for 10 more with `recent` = the queue's last ~50 started
  songs + everything still upcoming, and appends them as `Source::Mix`.
- **User edits win**: *Play next* and *Add to queue* entries are never
  removed by the mix; removing or reordering mix entries is kept. Turning
  the mix off or switching mixes drops only `Source::Mix` entries.
- **Exhausted**: the queue shows "This mix has run out" and stops asking
  until the mix changes.
- While a mix is on, starting a song does not add its album remainder.

**Saved mixes** are one Prefs value per user under `mix.saved`: a capnp
`SavedMixes` message, so every first-party client reads the same list.

## Observability (ODD)

Named signal before the fix: none existed (there was no mix).

- `pharos_mix_total{outcome}`: `full`, `short`, `empty`, `invalid`,
  `seed_not_found`, `unavailable`. Every call, success or not.
- `pharos_mix_relaxed_total{step}`: `repeat`, `share`, `spacing`.
- `pharos_mix_seconds`: time per call, pool read included.
- Log `mix: batch picked` (INFO) with `rules`, `seed`, `asked`, `picked`,
  `pool`, `eligible`, `exhausted`, `relaxed`, `elapsed_ms`; `mix: no batch`
  (WARN) with the error naming the offending rule and value.
- Queries: `sum by (outcome) (rate(pharos_mix_total[1h]))`;
  `{namespace="pharos"} |= "mix: batch picked" | logfmt | exhausted="true"`.
- Client (slice 2): `pharos: mix top-up asked= got= exhausted= relaxed=`.

## Limits and cost

Each call reads every song the user may hear plus their play state. That's
fine for a home library of tens of thousands of songs, and
`pharos_mix_seconds` shows when it stops being fine. The fix then is a
pushdown of the hard rules into the store query, not a change to the wire.

## Slices

1. Engine, `Library.mix` (schema v9), `SavedMixes`, `pharos-client` mix
   types and presets, tests, metrics. *(this PR)*
2. UI: "Mix from this song" on song rows and the player, a mix picker
   with presets and saved mixes, a rule editor, queue top-up, reasons in
   the queue panel.
3. Later: a similar-artists source behind an adapter trait, store
   pushdown if the call gets slow. Tempo and energy are read from the
   probe once B445 lands (a one-line change in `services::mix::song`).
