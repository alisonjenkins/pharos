# Spec 038 — UX round 2 (audit of 2026-11-10)

- **Status:** Draft. No code yet. Decisions marked **[ALISON]** are open and
  block only the requirement that names them.
- **Date:** 2026-11-10T11:00:00Z
- **Source:** static UX audit of `main` at `c96f8eb` (2026-11-10). Nothing was
  built or run; the live deployment was not reachable from the audit machine.
  Requirements whose evidence is "computed from CSS" or "read on the TV line"
  say so, and the first task of each group is to reproduce the problem.
- **ADRs:** to be written with the change that makes the decision: the Next Up
  rule (G1), the minimum resume point (G1), the TV line convergence (G6).
- **Claimed ids:** B1224–B1283, V1187–V1246, ADR 0113–0115 (entries go into `001-pharos-baseline/bugs.md` /
  `invariants.md` with the fix, not before). Split per group in `tasks.md`.
- **Predecessor:** the 2026-11-08 audit (B805–B839). Thirteen of its fifteen
  top picks are fixed on main. Its still-open items are folded into the groups
  below and keep their old audit ids in brackets (A9, B6, C8, …).

## Problem

The first audit removed dead ends and silent failures. What is left is a
different class: the product answers, but in the wrong place or for the wrong
device.

- Home's Next Up rail is not "the next episode of what I am watching".
- A show page opens on its first season whatever the viewer has watched.
- The seek bar and the player controls assume a mouse.
- The TV app cannot search, cannot press Skip in one go, and forgets its place.
- Waiting and failing each have one message for every cause.

## Scope and rules

- Seven groups, G1–G7, each sized to be one thread. Groups are independent
  unless a requirement says otherwise.
- **Video dominates the player.** No requirement adds permanent chrome over
  the picture.
- **The music player's look and layout do not change.** G2 and G7 touch music
  browse and music state only. Anything that would alter the listen view is
  listed under Deferred with **[ALISON]**.
- **Performance may only improve.** Any change that spends RAM or CPU to buy
  speed is listed under Deferred with **[ALISON]** and is not built from this
  spec.
- **ODD.** Each group names its signal. Where the audit found none, the first
  commit of the group adds it, separately from the fix.
- Fixtures and examples use invented titles only.

## G1 — Home rails and resume rules (server)

### Problem

1. **Next Up lists shows nobody started.** The rail buckets every unplayed
   episode of every visible series, picks each series' lowest, sorts by series
   name and truncates at 24 (`pharos-server/src/services/library.rs:321-403`,
   `:1336`). A show being watched late in the alphabet never appears. A
   season-0 special wins as "lowest". An episode in progress shows in both
   Continue Watching and Next Up. The scan also binds every library id into
   `user_data_bulk` [D5].
2. **Latest rails are not grouped.** `latest` is a plain `created_at DESC
   LIMIT` (`services/library.rs:147-207`, called at `:1365-1369`). One new
   season or album fills the whole rail with its own episodes or tracks.
3. **Any start counts as in progress.** A stop with position > 0 and not
   finished is stored and listed (`services/play_state.rs:270-282`,
   `pharos-store-sqlx/src/postgres.rs:4101-4103`). There is no minimum.
4. **A stop whose user-data read fails is dropped silently**
   (`services/play_state.rs:260-274`): no log, no count, no denial.

### Requirements

- **FR-101** Next Up holds one episode per series the viewer has *started*: a
  series with at least one played or in-progress episode. A started series
  with no eligible episode under FR-102 (everything after the last played
  one is watched, or nothing follows it) is left out.
- **FR-102** The episode is the first unplayed episode after the most recently
  played one in airing order. Season 0 is considered only when the viewer's
  last played episode is itself in season 0.
- **FR-103** Order is by the series' most recent play, newest first.
- **FR-104** An episode already in Continue Watching is not repeated in Next Up.
- **FR-105** The query is driven from the viewer's `user_data` rows, not from
  a full-library scan [D5]. It must not be slower than today at p95.
- **FR-106** A library's latest rail shows one tile per series (TV) or album
  (music), dated by its newest child, with a count of new children when more
  than one. Films are unchanged. The grouping key is the series identity key
  of spec 033.
- **FR-107** A stop below the minimum resume point stores no position and
  leaves the item out of Continue Watching. The minimum is a setting beside
  `played_threshold_pct`. **[ALISON]** values: proposed for video the larger
  of 2% and 60 s; proposed for music, never list a song as resumable under
  60 s. Audiobooks and books are exempt.
- **FR-108** A failed read or write on the stop path is logged with its cause,
  counted, and returned to the client as a denial it can retry.
- **FR-109 [ALISON]** A Continue Watching or Next Up tile plays at once, with
  a separate info control for the title page (today both rails open the title
  page: `pharos-ui/src/native/views.rs:704`, `:1027-1052`). Continue Reading
  already opens the reader in one step. Not built until answered.

### Signal

- New: `pharos_next_up_total{outcome}` (`listed`, `empty`, `error`) with the
  number of series considered and kept on the debug line.
- New: `pharos_play_stop_total{outcome,reason}` covering `saved`, `finished`,
  `below_minimum`, `store_error`.
- Existing: `pharos_store_query_seconds{method="user_data_bulk"}` must fall
  for the Next Up route.

## G2 — Show page and browse (web UI)

### Problem and requirements

Paths are under `crates/pharos-ui/`.

- **FR-201** A show page opens on the season that holds the viewer's next
  unwatched or in-progress episode, and the hero's Play starts that episode.
  Today `default_season` is the first season above 0 and Play falls back to
  the first episode of the season shown (`src/native/browse.rs:440-446`,
  `:561`, `:1735-1741`). A viewer with nothing watched still lands on season 1.
- **FR-202** Below 40rem an episode row stacks: name (up to two lines) over
  its controls, and the two queue buttons fold into one "more" control. Every
  target is at least 44px. Today the row is one flex line with a `nowrap` name
  (`assets/pharos.css:3781-3835`, `:3916-3918`; `browse.rs:1930-1965`) [A17].
  Evidence is computed from CSS: reproduce at 390px first.
- **FR-203** A music library's Artists and Albums grids have the letter rail
  the TV and film grids have, and Albums has a sort (`browse.rs:2048-2069`,
  mixed libraries `:724`; the index call exists at `:1159`).
- **FR-204** The music library tab is kept in the URL, and the tabs take arrow
  keys (`browse.rs:2008-2044`) [A21].
- **FR-205** The series page loads its independent calls together, not one
  after another (`browse.rs:560-567`) [C8].
- **FR-206** Home and title pages render their last content at once on Back
  and restore the scroll position while they refresh
  (`src/native/app.rs:3697-3719`) [A11].
- **FR-207** Search has a URL (`/find?q=`). Back from a result returns to the
  results. "See all" groups hits by kind and either pages them or states the
  cap (`src/native/route.rs:74`, `app.rs:2487-2491`, `find.rs:10`,
  `palette.rs:153-156`).
- **FR-208** The library filter selects are reachable with arrow keys on web,
  as they already are on the TV line (`src/native/web.rs:1805`) [A9].
- **FR-209** Manage keeps its page in the URL (`src/native/manage.rs:197-207`).
- **FR-210** Empty and failed states say what to do: the empty home is worded
  by role and names where libraries are configured (`views.rs:632`); an empty
  library shows "Nothing here yet" instead of a bare heading
  (`views.rs:661-669`) [A18]; a failed paging step shows its reason beside the
  retry (`browse.rs:1314-1320`) [A20]; a filter change shows a busy state
  (`browse.rs:838-840`) [A19]; the not-found page offers Home and Search
  (`app.rs:2186`) [A23]; library pills, breadcrumbs and the account name retry
  once and say when they could not load (`app.rs:1882`, `:1900`, `:1958`)
  [A15].
- **FR-211** One-click destructive or silent actions follow the existing
  two-step confirm and notice patterns: a failed description-language save
  (`app.rs:2976-2990`), the hand-fix Remove in Manage
  (`src/views/admin.rs:1988-1993`). The palette lists Manage only for admins
  and gains Together, Playback and Account (`palette.rs:101-112`).
- **FR-212** The hero image is requested at the size it is drawn, through the
  shared `Art` component (`browse.rs:1589`) [C7].
- **FR-213** The palette uses dynamic viewport units and shows a focus ring on
  its input (`assets/pharos.css:398`, `:411`, `:429`) [A24]. What's New
  reports a failed save honestly and its choices take arrow keys
  (`whats_new.rs:445-446`, `:515`, `:610`, `:623`) [A13, A14].

### Signal

Client-side behaviour; the proof is an SSR or browser test per requirement.
FR-205 is visible server-side as overlapping `rpc.library.*` spans for one
navigation in Tempo.

## G3 — Player controls by touch and pointer

### Problem and requirements

- **FR-301** The seek bar scrubs by drag and by touch, issues one seek on
  release (one group command in a group), shows the buffered range, and is
  focusable with arrow-key steps. Today it handles mouse click and hover only,
  with `touch-action: none` and a `role="slider"` that has no tabindex
  (`src/native/watch_view.rs:1208-1223`, `assets/pharos.css:2361`) [B6].
  The trickplay preview follows the scrub on touch.
- **FR-302** On a narrow screen the control bar keeps play, time, seek and
  fullscreen, and the seven menus (Tracks, Audio, Subtitles, Size, 3D,
  Version, Quality) move behind one settings control that opens a sheet. The
  chrome must not cover more than one line of the picture
  (`watch_view.rs:961-1076`, `pharos.css:2304-2350`, `:2508-2510`).
- **FR-303** On coarse pointers every player control is at least 44px
  (`pharos.css:2313-2315`, `:2343-2345`, `:2356-2361`). A double-tap on the
  left or right third seeks 10 s; a double-click toggles fullscreen.
- **FR-304** The mouse wheel changes volume only in the full view and ignores
  trackpad pixel deltas (`watch_view.rs:786-789`, `:1750-1761`).
- **FR-305** Player glyphs are the SVG icons the stage bar uses, not text or
  emoji (`watch_view.rs:867`, `:967-984`, `:1074`, `:1303`;
  `stage.rs:748-751`).
- **FR-306** The small player's progress line seeks on click
  (`stage.rs:731-733`).
- **FR-307** A speed control (disabled inside a group, where drift correction
  owns the rate) and a subtitle delay in 0.1 s steps [B15].
- **FR-308 [ALISON]** Key map: whether `k`/`j` become play and seek-back, as
  on the large video sites, instead of volume (`keymap.rs:312-323`) [B16].

### Signal

FR-301 in a group: one `Groups.command` per scrub, visible as the group
command counter. The rest is client-only and proved by browser tests at
390x844 with touch.

## G4 — Player feedback: waiting, failing, Up Next

### Problem and requirements

- **FR-401** The wait caption names its cause: starting, seeking, buffering,
  server busy, waiting for a named group member. Today every wait reads
  "Waiting for the server…" (`watch_view.rs:841-850`) [B18].
- **FR-402** The server tells the client when a play or seek is queued behind
  busy encoders, and the client must be able to learn it *while* the request
  is still pending: a load hint on the grant, a push event, or a cheap status
  call. A figure on the segment response alone arrives after the wait has
  ended and does not meet this. `queue_wait_ms` is recorded server-side only today
  (`pharos-transcode/src/scheduler.rs:1863-1931`). No new per-session state
  on the server beyond what the scheduler already holds.
- **FR-403** The failure panel picks its hint from the failure kind (network,
  refused by the server, cannot be played by this browser) and carries a
  Report a problem button; the feature exists but is not on the panel
  (`src/native/views.rs:549-607`, `app.rs:4513-4516`, `:4757-4763`,
  `palette.rs:35`).
- **FR-404** Every native RPC denial carries the call's trace id, and error
  panels show it small. The wire field exists and is never set
  (`pharos-api/schema/pharos.capnp:44`, `pharos-api/src/instrument.rs:416-457`).
  Store faults keep their distinct cause on the server log line.
- **FR-405** When credits were not detected, Up Next is a small pill until the
  last 15 s or the real end, not the full card for the last 5% of the runtime
  (`src/native/playback.rs:2341-2345`, `:2371-2377`, `:2422-2426`).
- **FR-406** In the floating window and the mini bar, Up Next is one line in
  the stage bar ("Next in 12 s · Cancel"), not the full card
  (`assets/pharos.css:4819-4822`, `:2525-2540`).
- **FR-407** A refused autoplay is detected and shown as "Tap to start"; in a
  group the silent retry stops until a gesture (`src/native/web.rs:486-507`,
  `group_session.rs:1730-1750`) [B14].
- **FR-408** The full view's first load shows Back with the loading state
  (`app.rs:3661-3664`, `:4740-4741`).

### Signal

- Existing: `pharos_transcode_pending_jobs`, `pharos_transcode_queue_wait_seconds`.
- FR-404 is proved by query: a denial's trace id read off the UI finds its
  line with `{namespace="pharos"} | json | trace_id="<id>"`.
- New client event for FR-407 (`autoplay_refused`) through the existing
  telemetry path, opt-in as today.

## G5 — Watching together feedback

- **FR-501** A failed rejoin is a timed status note, not a red alert pinned
  over the picture for the rest of the film (`app.rs:1743`, `:6766-6768`).
- **FR-502** While any member is buffering, a persistent "Waiting for <name>"
  pill shows whether or not the chrome is up, and clears with a "ready" line.
  Today it is a 4 s toast (`src/native/activity.rs:12`, `:86`;
  `together_view.rs:86-87`) [B7].
- **FR-503** A lost connection shows inside the full player, not only in the
  hidden shell (`app.rs:1979`) [B9]. The thread first checks whether an active
  group session rebinds to the new socket after a reconnect.
- **FR-504** The Together page says what each group is watching, refreshes
  while open (from server push), has Try again on failure, and its empty state
  tells the viewer how to start a group (`together_view.rs:14-41`,
  `app.rs:2765-2814`).
- **FR-505** Leave has an undo for 5 s (`together_view.rs:129-134`) [B20].

Signal: existing group command and member-state metrics; FR-503 adds a client
event on reconnect inside a group.

## G6 — TV with a remote

**Precondition.** The TV app is on two lines. Main cannot play in the Android
build (`android_page.rs:259-283`, `:700`); `feat/tv-android-playback` plays
but lacks the scrub seek and its Back guard. G6 starts by converging them
(FR-601). Requirements below cite the TV line as `TV:` and were read there,
not run on a TV.

- **FR-601** One line: the TV playback line lands on main in reviewable
  slices, and CI compiles the `mobile` feature so the Android page cannot rot.
  Recorded in an ADR.
- **FR-602** Search is reachable: a focusable Search control in the TV banner
  opens the palette with its input focused, and the shortcut hint is not shown
  on TV (`TV:android_page.rs:1555-1556`, `TV:app.rs:2227`).
- **FR-603** Back from a title returns to the tile it was opened from
  (`TV:android_page.rs:879-923`, `TV:browse.rs:1056-1076`).
- **FR-604** While Skip or Up Next shows and focus is on the picture, OK
  activates it; Back cancels Up Next before it leaves the player
  (`TV:watch_view.rs:557-566`, `TV:tv_page.rs:274`, `keymap.rs:452`).
- **FR-605** Leaving the app reports the position and pauses
  (`TV:android_page.rs:934-938`, `:578`).
- **FR-606** Text fields, checkboxes and sliders are reachable and leavable
  with the D-pad (join code, diagnostics opt-in, mix editor)
  (`together_view.rs:167`, `privacy.rs:39`, `mixes.rs:811-961`).
- **FR-607** Back closes an open dialog or menu first
  (`TV:android_page.rs:1870-1879`, `:762`).
- **FR-608** The TV sends client telemetry when the viewer opted in
  (`TV:android_page.rs:1332-1341`).
- **FR-609** Fast-forward, rewind, next, previous and stop keys work
  (`TV:android_page.rs:1636-1637`, `:774`).
- **FR-610** The out-of-date notice shows the installed and required versions
  and how to update (`TV:android_page.rs:218`, `TV:app.rs:1666-1675`).

Signal: FR-608 is the signal for the rest; until it lands a TV playback
failure cannot be seen from the server.

## G7 — Admin and settings

- **FR-701** A scan shows progress and a result. The UI consumes the Task push
  topic the server already publishes (`pharos-server/src/services/events.rs:378`;
  dropped at `pharos-ui/src/native/live.rs:87`): a progress bar on Libraries
  and Tasks, and "added N, changed M, removed K" at the end. A single library
  can be scanned alone (`pharos-ui/src/views/admin.rs:1035-1072`,
  `views/app_state.rs:996-1000`).
- **FR-702** Settings → Playback has preferred audio language, preferred
  subtitle language and subtitle mode; the server's selection rules read them
  (`pharos-ui/src/native/playback_settings.rs:64-147`). A per-show pick still
  wins.
- **FR-703** Sign out asks first when books are kept on the device, naming the
  count; Remove on a kept book is two-step while offline and shows a failure
  (`src/native/account.rs:102-108`, `app.rs:1993-2006`, `kept_view.rs:92-97`,
  `kept_web.rs:452-462`).
- **FR-704** The music queue survives a reload, as the current song already
  does (`app.rs:1762`, `stage.rs:275`). State only; the queue's look and rules
  (V375) do not change.
- **FR-705** One shared notice when the browser cannot store choices, used by
  the listen view's stored choices as Appearance uses it (`listen.rs:76`,
  `:216-251`; `stage.rs:448`). **[ALISON]** because it adds a line to the
  music screen.
- **FR-706** The visualiser does not wake every frame while paused
  (`listen.rs:1022-1025`) [C9]. No visible change.

Signal: FR-701 adds `pharos_scan_total{outcome}` with counts on the end line
if the scan path does not already expose one; check first.

## Deferred, with the decision each needs

| Item | Why deferred |
|---|---|
| Image placeholders (blurhash or dominant colour; the schema field exists, `pharos.capnp:71`) | **[ALISON]** costs bytes per item in rows and lists |
| Image cache eviction by last hit (`pharos-cache/src/image_cache.rs:1324`) [D15] | **[ALISON]** needs an in-memory hit map |
| Auth result cache [D9] | **[ALISON]** RAM, and needs revocation rules |
| Auto quality forgetting a lowered rung between videos (`app.rs:4272-4276`, `quality.rs:129-131`) | **[ALISON]** playback trade-off |
| A note on the listen view when the audio tap fails (`listen.rs:125`) | **[ALISON]** music screen |
| Favourites past 60 per kind (`favourites.rs:13`) | Owned by the collections and favourites work |
| Trigram search index [D10], segment cache eviction scan [D13], sweeps loading the full library [D14] | Not felt at the current library size; memory budget work owns D14 |

## Out of scope

- Stream retry and recovery timing (spec 031).
- Subtitle renderer work (specs 026, 028).
- 10-foot text density on TV (its own thread).
- The book reader's internals.
