# Feature Specification: browse and discover in pharos-ui

**Feature directory**: `specs/012-ui-browse/`
**Created**: 2026-11-01
**Status**: draft (`/speckit-specify` stage; next: `/speckit-clarify`)
**Input**: "It must support all the things Jellyfin's UI does and more."
(Alison, 2026-11-01)
**Depends on**: 009 (router, data layer, GPU rails), 010 (tokens), 011 (focus,
actions). Server endpoints are already DONE per
`docs/jellyfin-web-feature-matrix.md` unless marked.
**API**: pharos-ui calls the pharos-native API (017, ADR-0028). Jellyfin
endpoints named below identify the existing server behaviour that a native
method wraps through a shared domain service (V172); they are not what the UI
calls. Jellyfin clients keep using them unchanged.
**Supersedes**: T52 roadmap rows for Library browse / Item detail / Search, T54.

## Goal

A Netflix-grade browsing experience over every pharos media kind (Movie,
Episode/Series, Audio, Book) and Live TV, at jellyfin-web parity for what it
can show and filter, and faster to move around in.

## User Stories

### US1 — home (P1)

Home opens with a hero (rotating featured/next-up item with backdrop, logo,
overview, Play / More info) and rails: Continue Watching, Next Up, Continue
Reading (books), Recently Added per library, Live now (if Live TV), Favourites,
Because you watched …, plus My Media library tiles. Users reorder, hide and add
rails.

**Endpoints**: `Users/{id}/Views`, `Items/Resume`, `Shows/NextUp`,
`Items/Latest`, `Items/{id}/Similar`, `LiveTv/Programs/Recommended`,
`DisplayPreferences` (home sections).
**Independent test**: seed one resumable film, one next-up episode, one
part-read book; all three rails render with progress; reordering persists.

### US2 — library pages (P1)

A library shows a virtualised poster grid (or list/table view) with sort
(name, date added, release, rating, runtime, play count, random), filters
(played, favourite, genre, year, rating, resolution/HDR, audio language,
subtitle language, studio, tags, kind-specific: author/series for books),
letter jump, and saved views. Sort/filter state lives in the URL.

**Endpoints**: `Users/{id}/Items` (`SortBy`, `Filters`, `NameStartsWith`,
paging), `Items/Filters`, `Genres`, `Studios`, `Persons`.

### US3 — shows (P1)

Series page with seasons as tabs, episode list with stills, runtime, played
state and per-episode progress; mark season/series played; "Play next unwatched".

### US4 — films and extras (P1)

Detail page: backdrop + logo hero, overview, year, rating, runtime, ends-at,
genres (links), studios, tech badges (4K, HDR/DV, Atmos, codec), audio/sub
languages, version picker (multiple files), cast & crew rail, chapters with
trickplay thumbnails, similar items, collection it belongs to, external links,
trailer if present.

### US5 — music (P1)

Music library tabs: Albums, Album Artists, Artists, Songs, Genres, Playlists.
Album page with track list (disc grouping, duration, play/queue per track),
artist page (albums, top tracks, similar artists, MusicBrainz info), instant
mix `[server: InstantMix MISSING, T76]`.

### US6 — books (P1)

Books library grouped by author and series (series index order, missing index
last), covers, format badge (epub/pdf/comic/converted-Kindle), Continue Reading,
"Read" opens the in-app reader (013 US6). DRM-locked Kindle files shown as
not readable, with reason (005).

### US7 — people, genres, studios (P2)

Person page with photo, bio, filmography grouped by role; genre and studio hubs
as rails per library.

### US8 — search (P1)

Instant results as you type (`Search/Hints`), grouped by kind, keyboard
navigable, with suggestions (`Search/Suggestions`) when empty and recent
searches. Also feeds the 011 palette.

### US9 — collections and playlists (P1)

Browse collections (BoxSets) and playlists; create, rename, delete; add/remove
items from any item's context menu; drag to reorder playlist items.
**Server**: collections DONE; playlists CRUD MISSING (T70) — this story depends
on T70.

### US10 — Live TV guide (P2)

Channel list with logos and now/next, a horizontally scrolling EPG grid
(GPU-rendered for smooth scroll) with time ruler, favourites/groups filter,
"Watch" opens the live player (013 US5). DVR (recordings/timers) dropped per
gap assessment D10 pending confirmation.

### US11 — item actions everywhere (P1)

Every card and detail page exposes the 011 action set: play, resume/start over,
favourite, played/unplayed, add to playlist/collection, play in group (014),
open in jellyfin-web, refresh metadata / edit / identify (admin, 015), delete
(admin, confirm), download file.

### US12 — "more" beyond jellyfin-web (P2)

- **Hover/focus preview**: after a short dwell, the card expands to backdrop +
  trickplay loop or still, with quick actions (Netflix-style).
- **Watch-a-URL tile** on home for users allowed remote sources (008, 014).
- **Tech filter chips** (HDR, 4K, Atmos, direct-play-safe for this device).
- **Random pick** ("Surprise me") honouring current filters.

## Functional Requirements

- **FR-001** Rails and grids drawn on the 009 GPU layer with DOM cards for focus,
  text and a11y; virtualised; prefetch the next page at 70 % scroll.
- **FR-002** Card variants: portrait poster, landscape thumb, square (music),
  book cover, channel logo; badges for progress, played, favourite, unaired,
  new, group-active.
- **FR-003** Every list view is a URL with its sort/filter/view state (009 FR-001).
- **FR-004** Saved views per library per user (DisplayPreferences).
- **FR-005** Home section config compatible with jellyfin-web's
  `DisplayPreferences` home keys, so both clients agree where they overlap.
- **FR-006** Empty, loading (skeleton) and error states for every view.
- **FR-007** Parental-rating and folder-access filtering is server-side (T68); the
  UI must not show counts or tiles for items the server withholds.
- **FR-008** Image types used: Primary, Backdrop, Logo, Thumb, Banner, person
  Primary, channel Primary; missing types degrade to generated art (title on
  theme gradient), never a broken image.

## Success criteria

- **SC-001** Every row of the feature matrix's Home, Libraries & views, Item
  detail, Search and Collections & playlists sections is reachable in pharos-ui
  (Playwright test per row).
- **SC-002** 1,000-item library grid scrolls at 60 fps (009 NFR-003).
- **SC-003** Keyboard-only path to any item in ≤ 4 actions via the palette.

## Edge cases

- Series with specials (season 0), absolute-numbered anime, missing episodes.
- Items with no images at all; very long titles; RTL titles.
- Libraries mixing kinds; a library with 50 k tracks.
- Synthetic album ids (`synth_album_dto`) — must resolve like real ones.
