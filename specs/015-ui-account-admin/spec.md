# Feature Specification: account, preferences, metadata editing and admin

**Feature directory**: `specs/015-ui-account-admin/`
**Created**: 2026-11-01
**Status**: partially built (audited 2026-11-09): sign-in, account/password/avatar, Quick Connect approve, Manage (users, libraries, devices, activity, tasks, plugins, logs, API keys, branding, duplicates, integrity, metadata) shipped; metadata editing slice 1 built (fields), images/people/genres editing, dashboard overview, first-run sheet open. See Progress
**Input**: "… support all the things Jellyfin's UI does … tailored to the Pharos
server (there are some things we might drop)." (Alison, 2026-11-01)
**Depends on**: 009–011; server rows T67, T68, T69, T70, T73, T74, T76
**API**: pharos-ui calls the pharos-native API (017, ADR-0028). Jellyfin
endpoints named below identify the existing server behaviour that a native
method wraps through a shared domain service (V172); they are not what the UI
calls. Jellyfin clients keep using them unchanged.
**Supersedes**: T55, T58, T59 roadmap rows (their shipped panes migrate onto the
design system rather than being rewritten).
**Drops**: see `docs/ui-gap-assessment.md` §3 (D1–D18), pending Alison's
confirmation.

## User Stories

### US1 — sign in (P1)

Login with user avatars (`Users/Public`) or manual name, Quick Connect (both
directions), multiple saved servers, remember-me per device, logout, and a
themed login screen (010 US5). No forgot-password flow (D3).

### US2 — profile and preferences (P1)

Profile (avatar upload, password change), and preference pages: Appearance
(010), Keyboard (011), Home (012 US1), Playback (max bitrate, prefer direct
play, resume/played thresholds, auto-play next, skip-segment behaviour 013 US3),
Subtitles (default mode, languages, style), Audio (languages, default-track
preference), Reading (reader defaults), Language/locale. All stored server-side
and shared with jellyfin-web where a Jellyfin key exists.

### US3 — metadata editing (P2, admin)

Edit item fields (title, sort title, overview, dates, ratings, genres, tags,
people, provider ids), image manager (view, upload, delete, reorder; remote
image search deferred D18), refresh metadata (choose mode), identify against
TMDB/MusicBrainz, manage subtitles (upload/delete) and lyrics.
**Server**: `POST Items/{id}`, identify/remote search and subtitle upload are
MISSING (T67/T76) — this story depends on them.

### US4 — dashboard overview (P1, admin)

One screen: server version/uptime, active sessions with what they play and how
(direct/transcode + reason), transcode devices (capacity, in use, cooldown,
learned allowance — 006/007), scan status, background I/O gate state, recent
activity, storage of caches. Live via websocket.

### US5 — users (P1, admin)

List/create/delete, admin flag, disable, hide from login, library access,
parental controls, session limits, feature flags (remote sources, live TV,
downloads, deletion, group sync). Depends on T68 for everything beyond admin.

### US6 — libraries (P1, admin)

List, add (folder picker via `Environment/DirectoryContents`), rename, add/remove
paths, scan now (per library / all), realtime-monitor toggle, see last scan
result and errors. Per-library fetcher ordering dropped (D7). Depends on T69.

### US7 — operations (P2, admin)

Scheduled tasks (list, run now, progress; triggers if T74 lands), logs (view,
tail, download), activity log (T73), devices (rename, revoke), API keys (exists,
T58), **cache tools** (segment purge by item and time range, quarantine list +
download — T120/T123), **transcode devices** read-only (D5), DLNA status
read-only (D14).

### US8 — first run (P3)

If the server has no users, pharos-ui shows a single "create admin, add first
library" sheet (replaces the wizard, D2).

### US9 — media management (P2, admin)

An admin finds what the library holds twice. First view (Alison, 2026-11-03):
**Duplicates** under Manage lists every film held as more than one item, each
with its versions' path, size, resolution, codec and date added, so the admin
can choose what to keep. Read-only: nothing is deleted from pharos (V285).
Served by `GET /Admin/Media/Duplicates` (admin-only) in the Manage panes; a
native method follows when Manage moves onto 017. Episodes, music and
deletion from this view are later stories.

## Functional Requirements

- **FR-001** Every admin page gated by `IsAdministrator` on both client and
  server; non-admins never see admin routes in the palette.
- **FR-002** Destructive actions (delete user/library/item, revoke, purge) use a
  confirmation that names the object and its scope, and are undoable where the
  server supports it (quarantine).
- **FR-003** Forms validate client-side from the same rules the server applies;
  server errors are shown against the field.
- **FR-004** Existing T55/T58/T59 views are migrated onto 009–011 (tokens,
  focus, actions) with their SSR tests kept green.
- **FR-005** Dashboard data (US4) shows the reason values the server records
  (ODD: the UI is a view over the same signals the dashboards query).

## Success criteria

- **SC-001** Every non-dropped row in the feature matrix's Auth, User
  management, Library management, Dashboard settings and Other admin sections is
  reachable in pharos-ui.
- **SC-002** Admin keyboard path: create a user and grant one library, palette
  only.

## Edge cases

- Last admin cannot remove their own admin flag or delete themselves.
- Library path not readable by the server: shown with the server's reason
  (`pharos_source_unreadable_total{reason}`).

## Progress (audited 2026-11-09)

- US1 sign-in: partial. Native sign-in takes name and password; Quick Connect approval is at `/settings/approve-device` (native/approve_device.rs). Guest Quick Connect and the saved-server picker (views/quick_connect.rs, views/server_picker.rs) exist only in the legacy app used when the native API is off, not on the native sign-in screen.
- US2 profile and preferences: partial. Avatar + password + language in native/account_settings.rs (`/settings/account`); Appearance, Playback (native/playback_settings.rs), Privacy routes exist. No dedicated Home, Subtitles-defaults, Audio-defaults or Reading preference pages found.
- US3 metadata editing: slice 1 built (2026-11-09, ADR-0097, V1030–V1032, migration 0078). Manage ▸ Metadata finds a film, episode, song, book or show and edits its title, sort title, overview, tagline, year, premiere date, parental rating and scores (a show: overview, premiere date, rating, score, genres); each edited field is locked against rescans and online refreshes until unlocked. Server: `GET`/`POST /admin/items/{id}/metadata`, and Jellyfin `POST /Items/{id}` through the same service. Identify already existed (`RemoteSearch/Movie|Series|Apply`; show identity hand fixes are spec 033, #1117). Open: item genres, tags, people and studios (join tables the scanner relinks), the image manager (upload, delete, reorder), refresh-mode choice, subtitle and lyrics management, `LockedFields` in Jellyfin item answers, and an Edit link from a title page.
- US4 dashboard overview: not started (no dashboard view; admin tabs show lists only; no uptime/sessions/transcode-device screen).
- US5 users: done as legacy admin pane (views/admin.rs `UsersPane`, `AdminUserRow`), served via native `/manage` (native/manage.rs). Parental controls / session limits / feature flags not verified.
- US6 libraries: partial (views/admin.rs `LibrariesPane`); folder picker, scan-now and last-scan result not verified.
- US7 operations: partial (Tasks, Logs, Activity, Devices, API keys tabs in views/admin.rs; cache tools, transcode devices, DLNA status not found).
- US8 first run: not started (no first-run sheet in pharos-ui).
- US9 media management: Duplicates (`DuplicatesSection`) and Damaged files / deep checks (`IntegritySection`, `DeepChecksSection`, #1057) done; episodes, music and deletion remain later stories.
- FR-004 migration onto 009-011 tokens: Manage pages still the views/admin.rs panes, not migrated.
