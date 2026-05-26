# Jellyfin parity audit

Source of truth for "what's left for Jellyfin client-compat". Drives T19.
Cross-ref `jellyfin-mapping.md` (architectural translation) and SPEC §V1,
§V7 (the invariants this doc protects).

Ground truth: Jellyfin OpenAPI **10.11.10** (`https://api.jellyfin.org/openapi/jellyfin-openapi-stable.json`).
Snapshot: 315 distinct paths × 388 (path, method) pairs across 62 OpenAPI tags.

## 1. Goal + scope

Phase 1 target: **client-compat parity**, not 1:1 reimplementation.
Deliverable: unmodified Finamp / Infuse / Jellyfin-{web,mobile,TV} log in,
browse, fetch artwork, direct-play (transcoded after T9), report
playback against pharos — no client-side patches (V1, V7).

Out of scope Phase 1:

- Server admin UI (user mgmt, scheduled-task triggers, plugin install,
  listing-provider config). Some GETs stubbed so clients don't 500;
  write paths defer past parity.
- Live TV, DLNA, channel plugins. See §6.
- Plex compat — T11–T14, separate axis.
- group-sync server algorithm — T15–T17. SyncPlay wire surface in §3.13
  is listed only to mark "owned by T17, not T19".

Done = Finamp + Jellyfin-web + Infuse open against pharos, browse, play,
report, with no obvious "image broken" / "search empty" / "session
ghost" regression vs real Jellyfin.

## 2. Status legend

| Glyph | Meaning |
|---|---|
| `✓` | Implemented and exercised by tests. |
| `~` | Partial — handler exists, returns a degenerate / stub shape. Clients accept it, deeper features absent. |
| `→` | Deferred to T19 (gap fill). Client visibly misbehaves without it. |
| `✗` | Won't-do in Phase 1. Tracked under §6 with reasoning. |

Glyphs are the only emoji used in this doc; they're load-bearing.

## 3. Endpoint groups

One sub-section per OpenAPI tag. Paths are Jellyfin-canonical — pharos
must match byte-equivalent (V7). `HEAD` listed only where load-bearing
(streaming, images); other HEADs handled implicitly.

### 3.1 System

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/System/Info` | GET | ✓ | Auth required. Returns `SystemInfoDto`. |
| `/System/Info/Public` | GET | ✓ | Anonymous. Currently same shape — confirm clients accept the auth'd shape unauthenticated. |
| `/System/Ping` | GET, POST | → | Trivial 200 / `"Jellyfin Server"` body. Used by clients as a health probe before auth. |
| `/System/Endpoint` | GET | → | Returns `IsLocal` / `IsInNetwork`. Some clients use to pick LAN vs WAN URL. |
| `/System/Logs` | GET | ✗ | Admin-only log browsing. Not used by playback clients. |
| `/System/Logs/Log` | GET | ✗ | As above. |
| `/System/Info/Storage` | GET | ✗ | Admin UI only. |
| `/System/Restart` | POST | ✗ | Admin only; pharos restarted out-of-band (systemd / kubectl). |
| `/System/Shutdown` | POST | ✗ | As above. |

### 3.2 User (and UserViews)

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/Users/AuthenticateByName` | POST | ✓ | T5. Argon2 verify + opaque token issue. |
| `/Users/Me` | GET | ✓ | T5. Returns the bearer's `UserDto`. |
| `/Users/AuthenticateWithQuickConnect` | POST | → | QuickConnect 6-digit code flow. Subsystem deferred (see §3.21). |
| `/Users/Public` | GET | → | Anonymous `(name, hasPassword, primaryImageTag)`. Finamp shows it on the login screen. |
| `/Users` | GET, POST | → | Admin list / create. Read needed by Jellyfin-web admin page; write strictly admin. |
| `/Users/{userId}` | GET, DELETE | → | Read needed for user pickers. |
| `/Users/New` | POST | → | First-run admin setup; for now create via CLI (`pharos admin user add`). |
| `/Users/Password` | POST | → | Required so clients change passwords without an out-of-band tool. |
| `/Users/Configuration` | POST | → | Per-user prefs (subtitle defaults, audio language). Clients write on settings save. |
| `/Users/{userId}/Policy` | POST | → | Admin-only. |
| `/Users/ForgotPassword` | POST | ✗ | Not in Phase 1; email recovery needs SMTP. |
| `/Users/ForgotPassword/Pin` | POST | ✗ | As above. |
| `/UserViews` | GET | → | Library list shown on the home screen. Currently absent — every client renders an empty home page. **High pri.** |
| `/UserViews/GroupingOptions` | GET | → | Used by Jellyfin-web library settings dialog. |

### 3.3 Items + UserLibrary + Library + LibraryStructure + Filter + Suggestions

These four OpenAPI tags overlap on `/Items` and `/Library` paths. Merged
here for clarity.

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/Items` | GET | ~ | T6 phase 1. Pagination by `StartIndex` / `Limit` works. Missing: every Jellyfin query knob (`SearchTerm`, `Filters`, `IncludeItemTypes`, `SortBy`, `Genres`, `Years`, `ParentId`, `Recursive`). Clients fall back to bulk-list — OK but slow. |
| `/Items/{itemId}` | GET | ~ | T6 phase 1. Bare `BaseItemDto`. Missing: `MediaSources` (T7 fills for stream targets), `MediaStreams`, `Chapters`, `ImageTags`, `BackdropImageTags`, `UserData`. **High pri** — without `UserData` every "resume" / "watched" UI is wrong. |
| `/Users/{userId}/Items` | GET | ~ | T6 phase 1. Same caveats as `/Items`. |
| `/Items/Latest` | GET | → | "Latest movies" / "Latest music" row on home screen. |
| `/Items/Root` | GET | → | Returns the synthetic root folder. Some clients walk from here. |
| `/Items/Counts` | GET | → | Dashboard widget; Finamp uses to show "N albums". |
| `/Items/Filters` | GET | → | Genre / year / tag facets to populate filter panels. |
| `/Items/Filters2` | GET | → | Newer schema; same purpose. |
| `/Items/Suggestions` | GET | → | Jellyfin-web "More like this" row. |
| `/Items/{itemId}/Intros` | GET | → | Pre-roll. Stub `[]` silences client retry. |
| `/Items/{itemId}/Ancestors` | GET | → | Parent chain. Breadcrumbs. |
| `/Items/{itemId}/{LocalTrailers,SpecialFeatures,ThemeMedia,ThemeSongs,ThemeVideos}` | GET | → | Stub `[]`. Saves clients several round-trips. |
| `/Items/{itemId}/CriticReviews` | GET | → | Stub `[]`; real impl post-parity (metadata provider). |
| `/{Items,Movies,Shows,Albums,Artists,Trailers}/{itemId}/Similar` | GET | → | "More like this". Single impl, genre-overlap heuristic OK. |
| `/Items/{itemId}/Download` | GET | → | Underlying file with `Content-Disposition: attachment`. Finamp offline mode uses it. |
| `/Items/{itemId}/File` | GET | → | Raw passthrough; admin-ish. Lower pri than `/Download`. |
| `/Items/{itemId}` | DELETE | ✗ | Destructive admin op. Defer past parity. |
| `/Items` | DELETE | ✗ | Bulk delete. As above. |
| `/UserItems/Resume` | GET | → | "Continue watching / listening" row. Needs `UserData` first. **High pri.** |
| `/UserItems/{itemId}/UserData` | GET, POST | → | Per-item watched / position. The single biggest user-visible gap once a client has played anything. |
| `/UserItems/{itemId}/Rating` | POST, DELETE | → | Thumbs-up. Cheap once `user_data` table exists. |
| `/UserFavoriteItems/{itemId}` | POST, DELETE | → | "Favorite" toggle. Same backing table. |
| `/Library/MediaFolders` | GET | → | List physical roots. Newer clients call this; older fall back to `/Library/VirtualFolders`. |
| `/Library/PhysicalPaths` | GET | → | Admin-ish; harmless to expose read-only. |
| `/Library/VirtualFolders` | GET | ~ | T6 phase 1. Returns one synthesized "All Media" entry. Real per-root mapping waits on media-roots wiring. |
| `/Library/VirtualFolders` | POST, DELETE | ✗ | Library add/remove from clients. Admin-only; CLI for now. |
| `/Library/VirtualFolders/{Paths,Name,LibraryOptions,Paths/Update}` | POST | ✗ | As above. |
| `/Library/VirtualFolders/Paths` | DELETE | ✗ | As above. |
| `/Library/Refresh` | POST | → | Trigger scanner. Lightweight to wire; useful test affordance. |
| `/Library/{Movies,Series,Media}/{Added,Updated}` | POST | ✗ | Inbound webhook from external metadata agents. Defer. |
| `/Libraries/AvailableOptions` | GET | → | Static dictionary; stub. |

`UserData` and `UserItems/Resume` together drive the "did this do
anything" perception of every client. Treat as one gap.

### 3.4 Videos / Audio / UniversalAudio (direct streaming)

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/Videos/{itemId}/stream` | GET, HEAD | ✓ | T7. actix-files `NamedFile` + range + ETag. |
| `/Videos/{itemId}/stream.{container}` | GET, HEAD | ✓ | T7. Container suffix ignored — bytes are bytes. |
| `/Audio/{itemId}/stream` | GET, HEAD | ✓ | T7. |
| `/Audio/{itemId}/stream.{container}` | GET, HEAD | ✓ | T7. |
| `/Audio/{itemId}/universal` | GET, HEAD | ~ | T7 wires the path; ignores `container`, `audioCodec`, `maxStreamingBitrate`, `transcodingContainer`. Falls back to direct play 100% of the time. Acceptable until T9. |
| `/Videos/{itemId}/AdditionalParts` | GET | → | Multi-part movie. Stub `[]`. |
| `/Videos/{itemId}/AlternateSources` | DELETE | ✗ | Admin edit. |
| `/Videos/MergeVersions` | POST | ✗ | As above. |

### 3.5 DynamicHls / HlsSegment / MediaInfo (transcoded streaming)

Whole group depends on T8 (transcode pipeline) + T9 (HLS endpoints).

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/Items/{itemId}/PlaybackInfo` | GET, POST | → | Client posts capabilities; server picks direct vs transcode and returns `MediaSources` array. **First call any player makes** — without it Infuse and Finamp Hi-Fi refuse to start. |
| `/Videos/{itemId}/master.m3u8` | GET, HEAD | → | T9. Multi-variant playlist. |
| `/Videos/{itemId}/main.m3u8` | GET | → | T9. Single-variant. |
| `/Videos/{itemId}/live.m3u8` | GET | → | T9. Transcoded live media. |
| `/Videos/{itemId}/hls1/{playlistId}/{segmentId}.{container}` | GET | → | T9. Segment fetch — fMP4 + TS. |
| `/Videos/{itemId}/hls/{playlistId}/stream.m3u8` | GET | → | Legacy HLS scheme. |
| `/Videos/{itemId}/hls/{playlistId}/{segmentId}.{segmentContainer}` | GET | → | Legacy segment. |
| `/Audio/{itemId}/{master,main}.m3u8` | GET, HEAD | → | T9. Audio HLS. |
| `/Audio/{itemId}/hls1/{playlistId}/{segmentId}.{container}` | GET | → | T9. |
| `/Audio/{itemId}/hls/{segmentId}/stream.{aac,mp3}` | GET | → | Legacy AAC/MP3 segment. |
| `/Videos/ActiveEncodings` | DELETE | → | Stop active ffmpeg. Needed once transcoder is real. |
| `/Playback/BitrateTest` | GET | → | Client speedtest. Serve N MiB of random bytes. |
| `/LiveStreams/{Open,Close}` | POST | ✗ | Live-stream session — Live TV path only. |

### 3.6 Session + PlayState

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/Sessions` | GET | ✓ | T10. Returns `SessionRegistry` snapshot. |
| `/Sessions/Playing` | POST | ✓ | T10. `SessionEvent::Started`. |
| `/Sessions/Playing/Progress` | POST | ✓ | T10. `SessionEvent::Progress`. |
| `/Sessions/Playing/Stopped` | POST | ✓ | T10. `SessionEvent::Stopped`. |
| `/Sessions/Playing/Ping` | POST | → | Keep-alive heartbeat. Currently silently 404s; harmless but logs noise. |
| `/Sessions/Capabilities` | POST | ✓ | T10. Accepted + discarded. |
| `/Sessions/Capabilities/Full` | POST | ✓ | T10. As above. |
| `/Sessions/Viewing` | POST | → | "User is on item X (not playing)" — drives `LastActivityDate`. |
| `/Sessions/Logout` | POST | → | Token revoke. Cheap; should land with token-store cleanup. |
| `/Sessions/{sessionId}/Command` | POST | → | Remote control: `Mute`, `VolumeUp`, etc. Required for cast / control flows. |
| `/Sessions/{sessionId}/Command/{command}` | POST | → | URL-encoded variant. |
| `/Sessions/{sessionId}/Playing/{command}` | POST | → | `PlayPause` / `Seek` / `NextTrack` against another session. |
| `/Sessions/{sessionId}/System/{command}` | POST | ✗ | "Reboot client OS" — only smart-TV clients ever respect it. |
| `/Sessions/{sessionId}/Message` | POST | → | Push toast to a client. Cheap once WS is up. |
| `/Sessions/{sessionId}/Viewing` | POST | → | Tell another session to navigate. Same axis as `Playing/{command}`. |
| `/Sessions/{sessionId}/User/{userId}` | POST, DELETE | ✗ | Shared-session multi-user; rare. |
| `/Auth/Providers` | GET | → | List configured auth backends. Currently just `BuiltinAuth`. Trivial stub. |
| `/Auth/PasswordResetProviders` | GET | → | Stub `[]`. |
| `/UserPlayedItems/{itemId}` | POST, DELETE | → | Mark played / unplayed. Same `user_data` row as `/UserItems/{itemId}/UserData`. |
| `/PlayingItems/{itemId}` | POST, DELETE | → | Legacy alias of `/Sessions/Playing*`. Forward to same actor. |
| `/PlayingItems/{itemId}/Progress` | POST | → | As above. |

### 3.7 Image + RemoteImage

Every client opens with a flood of image requests. Returning 404 is
correct but each miss renders a broken-tile placeholder — UI looks
instantly worse than real Jellyfin.

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/Items/{itemId}/Images` | GET | → | Lists available image types per item. Used by clients to decide what to request. |
| `/Items/{itemId}/Images/{imageType}` | GET, HEAD | → | Default-index image. Common types: `Primary`, `Backdrop`, `Thumb`, `Logo`, `Banner`. |
| `/Items/{itemId}/Images/{imageType}/{imageIndex}` | GET, HEAD | → | Specific index. Backdrops can have multiple. |
| `/Items/{itemId}/Images/{imageType}/{imageIndex}/{tag}/{format}/{maxWidth}/{maxHeight}/{percentPlayed}/{unplayedCount}` | GET, HEAD | → | Params-baked-into-path variant. `image` crate resizes on the fly. **Highest churn endpoint** — every list view requests dozens. Needs caching. |
| `/Items/{itemId}/Images/{imageType}[/{imageIndex}[/Index]]` | POST, DELETE | ✗ | Admin upload / delete / reorder. |
| `/Persons/{name}/Images/{imageType}[/{imageIndex}]` | GET, HEAD | → | Person poster. Lower volume than items. |
| `/Artists/{name}/Images/{imageType}/{imageIndex}` | GET, HEAD | → | Music browse uses this heavily. |
| `/{Genres,MusicGenres,Studios}/{name}/Images/{imageType}[/{imageIndex}]` | GET, HEAD | → | Genre / studio tile art. Stub OK. |
| `/UserImage` | GET, HEAD, POST, DELETE | → | Per-user avatar. Read for clients; write for admin. |
| `/Branding/Splashscreen` | GET, HEAD, POST, DELETE | → | Pre-login splash. Stub 404 acceptable until admin UI. |

For pharos, image storage = content-addressed cache under
`<data_dir>/images/<sha256>/<size>.<fmt>`. Resize via the `image` dep
(SIMD per §C). Source images either bundled with media (`folder.jpg`)
or pulled by metadata providers (T6 phase 2).

### 3.8 Branding

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/Branding/Configuration` | GET | → | Returns `LoginDisclaimer`, `CustomCss`. Web client renders empty disclaimer if 404. Trivial stub. |
| `/Branding/Css` | GET | → | Custom theme CSS. Empty string OK. |
| `/Branding/Css.css` | GET | → | Same payload, different `Content-Type`. |

### 3.9 ApiKey

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/Auth/Keys` | GET, POST | → | List / mint server-issued API keys. Admin UI only. |
| `/Auth/Keys/{key}` | DELETE | → | Revoke. Admin UI. |

Admin-only; lower priority than image / search / playback gaps.

### 3.10 Devices

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/Devices` | GET, DELETE | → | List / revoke registered devices. Admin UI. |
| `/Devices/Info` | GET | → | Per-device info (last seen, etc.). |
| `/Devices/Options` | GET, POST | → | Per-device customization (name, icon). |

Backing table already exists implicitly (T4 issues per-device tokens) —
just needs read endpoints.

### 3.11 DisplayPreferences

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/DisplayPreferences/{displayPreferencesId}` | GET, POST | → | Per-(user, client) UI state (grid sort, view mode). Clients save layout state here. Returning 404 makes the web client re-prompt every page. **Mid-pri.** |

### 3.12 Plugins + Package + ScheduledTasks + Configuration + Dashboard + Backup + ActivityLog + ClientLog

Admin / server-management surface. Mostly `✗` Phase 1 — pharos is
configured via TOML + CLI. Read-only stubs for the GETs load-bearing
clients hit pre-auth.

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/Plugins` | GET | → | Return `[]`. Some clients query at startup to decide which features to surface. |
| `/Plugins/{pluginId}/Configuration` | GET, POST | ✗ | No plugin model. |
| `/Plugins/{pluginId}[/{version}[/{Enable,Disable,Image}]]` | * | ✗ | As above. |
| `/Plugins/{pluginId}/Manifest` | POST | ✗ | As above. |
| `/Packages` | GET | → | Return `[]`. As `/Plugins`. |
| `/Packages/{name}` | GET | ✗ | |
| `/Packages/Installed/{name}` | POST | ✗ | |
| `/Packages/Installing/{packageId}` | DELETE | ✗ | |
| `/Repositories` | GET, POST | ✗ | |
| `/ScheduledTasks` | GET | → | Return `[]`. Web client polls. |
| `/ScheduledTasks/{taskId}` | GET | ✗ | |
| `/ScheduledTasks/Running/{taskId}` | POST, DELETE | ✗ | |
| `/ScheduledTasks/{taskId}/Triggers` | POST | ✗ | |
| `/System/Configuration[/*]` | GET, POST | ✗ | Admin write. |
| `/System/ActivityLog/Entries` | GET | → | Return `[]`. Dashboard polls. |
| `/web/ConfigurationPages` | GET | → | Return `[]`. Web-admin nav. |
| `/web/ConfigurationPage` | GET | ✗ | |
| `/Backup[/*]` | * | ✗ | Admin only. |
| `/ClientLog/Document` | POST | → | Accept-and-discard. Web client posts JS errors here; without it, console fills with retry storms. |
| `/Startup/*` | * | ✗ | First-run wizard. pharos provisions admin via CLI. |
| `/Environment/*` | * | ✗ | Directory browser for the web admin. CLI replaces. |
| `/Localization/{Countries,Cultures,Options,ParentalRatings}` | GET | → | Static dictionaries. Clients call once and cache. Stub with the JSON Jellyfin ships. |
| `/Dashboard/*` | * | ✗ | |

### 3.13 SyncPlay

Owned by T16 / T17, not T19. Listed here so the gap is visible at a
glance; do not fill from T19. Surface must remain Jellyfin-shaped (V20).
T16 phase 1 already shipped pharos-native `/sync/v1/ws`; the Jellyfin
`/socket` + `/SyncPlay/*` bridge is T16 phase 2 / T17.

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/SyncPlay/{New,List,{id},Join,Leave}` | * | → | T16/T17 — group lifecycle. |
| `/SyncPlay/{Play,Pause,Unpause,Stop,Seek,Ping,Ready,Buffering,SetIgnoreWait}` | POST | → | T16 — playback ctl. V3 / V19. |
| `/SyncPlay/{Queue,SetNewQueue,SetPlaylistItem,MovePlaylistItem,RemoveFromPlaylist,NextItem,PreviousItem}` | POST | → | T16 — queue ctl. |
| `/SyncPlay/{SetRepeatMode,SetShuffleMode}` | POST | → | T16. |

### 3.14 Search

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/Search/Hints` | GET | → | Single endpoint, big UX impact. Finamp's main screen opens with a search hint query. 404 sticks the app on the spinner ~3s. **High pri.** SQL `LIKE` over `name` initially; FTS5 index post-parity. |

### 3.15 Genres / MusicGenres / Studios / Persons / Artists / Years / Filter

Browse-axis collections. Cheap once `/Items` filter knobs exist.

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/Genres[/{genreName}]` | GET | → | Distinct list with item counts; detail = name + image. |
| `/MusicGenres[/{genreName}]` | GET | → | As `/Genres`, filtered to audio. |
| `/Studios[/{name}]` | GET | → | Distinct list. |
| `/Persons[/{name}]` | GET | → | Cast / crew rollup. |
| `/Artists[/{name}]` | GET | → | Music. Finamp leans on this heavily. |
| `/Artists/AlbumArtists` | GET | → | Album-artist subset (vs track-artist). |
| `/Years[/{year}]` | GET | → | Year facet. |

### 3.16 Subtitle

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/Videos/{routeItemId}/{routeMediaSourceId}/Subtitles/{routeIndex}/Stream.{routeFormat}` | GET | → | Sidecar extraction. Sidecar `.srt`: direct. Embedded: ffmpeg one-shot. |
| `/Videos/.../Subtitles/{routeIndex}/{routeStartPositionTicks}/Stream.{routeFormat}` | GET | → | Same with start-offset. |
| `/Videos/{itemId}/{mediaSourceId}/Subtitles/{index}/subtitles.m3u8` | GET | → | HLS subtitle playlist. T9. |
| `/FallbackFont/Fonts[/{name}]` | GET | → | List / serve bundled fonts for ASS rendering. |
| `/Videos/{itemId}/Subtitles` | POST | ✗ | Upload sidecar. Admin. |
| `/Videos/{itemId}/Subtitles/{index}` | DELETE | ✗ | Delete sidecar. |
| `/Items/{itemId}/RemoteSearch/Subtitles/{language}` | GET | ✗ | OpenSubtitles plugin path. |
| `/Items/{itemId}/RemoteSearch/Subtitles/{subtitleId}` | POST | ✗ | As above. |
| `/Providers/Subtitles/Subtitles/{subtitleId}` | GET | ✗ | As above. |

### 3.17 LiveTV — see §6 (✗)

41 endpoints. None implemented; none planned for Phase 1.

### 3.18 DLNA — see §6 (✗)

Removed from upstream Jellyfin in 10.11. Confirmed by absence from
OpenAPI snapshot. pharos follows suit.

### 3.19 Channels

"Channels" in Jellyfin == plugin-supplied virtual libraries (Twitch,
Trakt, etc.), not Live TV channels. All depend on the plugin system.

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/Channels[/{channelId}/{Items,Features}]` | GET | ✗ | Stub `[]` acceptable so the home-screen rail doesn't render. |
| `/Channels/{Features,Items/Latest}` | GET | ✗ | As above. |

### 3.20 Playlists + Collection

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/Playlists` | POST | → | Create user playlist. T6 phase 2. |
| `/Playlists/{playlistId}` | GET, POST | → | Read / update metadata. |
| `/Playlists/{playlistId}/Items` | GET, POST, DELETE | → | List / append / remove items. |
| `/Playlists/{playlistId}/Items/{itemId}/Move/{newIndex}` | POST | → | Reorder. |
| `/Playlists/{playlistId}/Users[/{userId}]` | GET, POST, DELETE | → | Per-user share controls. |
| `/Collections` | POST | → | Like a playlist but for movies. |
| `/Collections/{collectionId}/Items` | POST, DELETE | → | |

### 3.21 QuickConnect

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/QuickConnect/Enabled` | GET | → | Returns `false`. Both web and Finamp render the QC button regardless of 404, but a clean `false` removes the entry. |
| `/QuickConnect/{Initiate,Connect,Authorize}` | POST/GET | ✗ | Subsystem deferred. |

### 3.22 TvShows / Movies / Trailers / Suggestions / InstantMix

Type-specialized browse rails. Cheap variants of `/Items` once filters
work.

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/Shows/{seriesId}/{Seasons,Episodes}` | GET | → | |
| `/Shows/{NextUp,Upcoming}` | GET | → | Continue-watching rail + calendar. |
| `/Movies/Recommendations` | GET | → | Heuristic; stub OK. |
| `/Trailers` | GET | → | Same shape as `/Items`. |
| `/{Albums,Artists,Items,Songs,Playlists}/{itemId}/InstantMix`, `/{Artists,MusicGenres}/InstantMix`, `/MusicGenres/{name}/InstantMix` | GET | → | Generate a quick mix from album / artist / genre / track. Finamp uses it. |

### 3.23 ItemRefresh / ItemUpdate / ItemLookup

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/Items/{itemId}/Refresh` | POST | → | Re-run metadata for one item. |
| `/Items/{itemId}` | POST | ✗ | Edit metadata. Admin. |
| `/Items/{itemId}/{ContentType,MetadataEditor,ExternalIdInfos}` | * | ✗ | Admin / metadata chrome. |
| `/Items/RemoteSearch/*` | POST | ✗ | OpenMovieDB / TMDb search. Post-parity. |

### 3.24 MediaSegments / Trickplay / VideoAttachments / Lyrics

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/MediaSegments/{itemId}` | GET | → | Intro / outro / recap markers. Stub `[]`. |
| `/Videos/{itemId}/Trickplay/{width}/{tiles.m3u8,{index}.jpg}` | GET | → | Scrubbing preview tiles. Lower pri but cheap. |
| `/Videos/{videoId}/{mediaSourceId}/Attachments/{index}` | GET | → | MKV attachments (fonts). |
| `/Audio/{itemId}/Lyrics` | GET | → | LRC / plain lyrics. |
| `/Audio/{itemId}/Lyrics` | POST, DELETE | ✗ | Edit. |
| `/Audio/{itemId}/RemoteSearch/Lyrics[/{lyricId}]` | GET, POST | ✗ | Provider search. |
| `/Providers/Lyrics/{lyricId}` | GET | ✗ | Provider fetch. |

### 3.25 Misc one-offs

| Endpoint | Method | Status | Notes |
|---|---|---|---|
| `/GetUtcTime` | GET | → | TimeSync probe. Trivial. Some clients hit it before every play. |
| `/Tmdb/ClientConfiguration` | GET | ✗ | TMDb provider chrome. |

## 4. Non-endpoint feature areas

These are the subsystems behind the surface in §3. Most gaps live here.

### 4.1 Library scanning

| Aspect | Status | Notes |
|---|---|---|
| Walk roots, ffprobe metadata | ✓ | T3. |
| File-watch (inotify / kqueue) | → | One-shot per `pharos scan` today. T19 should add notify-based incremental. |
| Sidecar files (`.nfo`, `folder.jpg`, `artist.jpg`) | → | Scanner sees but doesn't parse. |
| Multi-root per-user permissions | → | `UserPolicy` has the field; scanner doesn't enforce. |
| External metadata providers (TMDb, MusicBrainz, TVDB) | ✗ Phase 2 | Trait shape ready (`MetadataProvider`, jellyfin-mapping §2). |

### 4.2 Transcoding

| Aspect | Status | Notes |
|---|---|---|
| Direct play | ✓ | T7. |
| ffmpeg subprocess wrapper | → | T8. V6 (no server crash on transcode failure) is the load-bearing invariant. |
| HLS segmentation | → | T9. |
| Format negotiation / device profiles | → | T9. Plan: TOML profiles (jellyfin-mapping §7), not XML. |
| Hardware accel (vaapi, nvenc, qsv, videotoolbox) | → | T9 enumerate; pick at runtime via probe. |
| Segment cache | → | T9. Disk + LRU. |

### 4.3 Image processing

| Aspect | Status | Notes |
|---|---|---|
| Source resolution (item images on disk) | → | Scanner doesn't extract / fingerprint yet. |
| Resize on the fly | → | `image` crate dep already present (§C). |
| Content-addressed cache | → | Layout: `<data>/images/<sha>/<w>x<h>.<fmt>`. blake3 (§C). |
| Backdrop / banner / logo / chapter / primary types | → | Type taxonomy matches Jellyfin's `ImageType` enum. |
| Trickplay tile sheets | → | Byproduct of T8/T9. |

### 4.4 Plugin system

| Aspect | Status | Notes |
|---|---|---|
| Runtime `.dll` loading | ✗ | No stable Rust ABI. See jellyfin-mapping §4. |
| Cargo-feature gated providers | → | Trait scaffolding exists; first provider lands post-parity. |
| `/Plugins` returning `[]` | → | Stub so clients don't blow up. |

### 4.5 OpenAPI publication

| Aspect | Status | Notes |
|---|---|---|
| Self-host OpenAPI document | → | Generate via `utoipa` derive on actix handlers. Useful for docs + testing. |
| `/openapi/jellyfin-openapi-stable.json` shim | → | Some health-check tools probe it. Optional. |

### 4.6 WebSocket `/socket`

Only Jellyfin endpoint outside the OpenAPI doc that load-bearing clients
open. Multiplexed JSON over one WS, auth via `?api_key=` (V8 hazard:
token logging — audit on impl). Status: → T19. Without WS, "now playing"
panel never populates and the dashboard polls forever.

Message types pharos must accept / emit:

- `SessionsStart` / `SessionsStop` — subscribe to `/Sessions` deltas.
- `LibraryChanged` — push on scanner write.
- `ActivityLogEntry`, `RestartRequired`, `ServerShuttingDown` — stub OK.
- `PlayState`, `GeneralCommand` — addressed to a session; powers remote control (§3.6 `/Sessions/{id}/Command`).
- `SyncPlay*` — owned by T17. T16 phase 1 already runs `/sync/v1/ws`; the Jellyfin-shaped socket bridges through.

### 4.7 Push notifications

Jellyfin posts to admin-configured webhooks (Gotify, Pushbullet,
Discord) via plugins. Status: ✗ Phase 2.

## 5. Recommended T19 fill order

Each step unblocks the next user-visible behavior. Each item lands
behind a TDD test (V11) and preserves V7 byte-equivalent shapes.

1. **Images** — `/Items/{itemId}/Images/{type}[/{index}]` + params-in-path variant. Highest-volume request; every list view is broken without it. Content-addressed cache + on-the-fly resize via `image`.
2. **UserData + Resume** — `/UserItems/{itemId}/UserData` GET/POST, `/UserItems/Resume`. New `user_data` table on `MediaStore`. Without it `/Sessions/Playing/Progress` writes nothing meaningful, and every "watched" / "continue" UI is wrong.
3. **UserViews + MediaFolders** — `/UserViews`, `/Library/MediaFolders`. Home screen renders a real library list.
4. **Search** — `/Search/Hints`. SQL `LIKE`. Finamp opens with a search query.
5. **`/Items` query knobs** — `SearchTerm`, `IncludeItemTypes`, `Filters`, `SortBy`, `ParentId`, `Recursive`. Add `/Items/Latest`, `/Items/Filters`, `/Items/Counts`. Unblocks every browse view.
6. **`/Items/{itemId}/PlaybackInfo`** — first call any player makes. A fixed "direct play" decision suffices until T8/T9.
7. **Browse facets** — `/Genres`, `/MusicGenres`, `/Studios`, `/Persons`, `/Artists`, `/Artists/AlbumArtists`, `/Years`. Distinct-with-counts; unblocks rails.
8. **Stub set** — `/Plugins`, `/Packages`, `/ScheduledTasks`, `/Branding/Configuration`, `/Branding/Css`, `/QuickConnect/Enabled`, `/Channels`, `/Auth/Providers`, `/Auth/PasswordResetProviders`, `/web/ConfigurationPages`, `/System/ActivityLog/Entries`, `/Localization/*`, `/Items/{id}/{Intros,LocalTrailers,SpecialFeatures,ThemeMedia,ThemeSongs,ThemeVideos,CriticReviews}`, `/MediaSegments/{itemId}`, `/Sessions/Playing/Ping`, `/GetUtcTime`, `/ClientLog/Document`. One match arm each, empty / static. Silences retry storms.
9. **DisplayPreferences** — JSON blob keyed by `(user, client, id)`. Restores web client UX state.
10. **TvShows + Movies rails** — `/Shows/{NextUp,Upcoming}`, `/Shows/{id}/{Seasons,Episodes}`, `/Movies/Recommendations`, `/Items/Suggestions`, `/InstantMix/*`. Feeds off items 2 + 7.
11. **WebSocket `/socket`** — `actix-ws`. `SessionsStart` / `Stop` + `LibraryChanged` first. Hooks into `SessionRegistry` actor + scanner.
12. **User self-service** — `/Users/Password`, `/Users/Configuration`, `/Users` GET list. Create/delete stays on CLI.
13. **Subtitles** — sidecar extract + serve. Embedded via ffmpeg lands with T8.
14. **Playlists + Collections** — needs new tables. Self-contained.
15. **Devices + ApiKey + ActivityLog writes** — admin. Last; no client breaks without them.

Items 1–5 = the bulk of user-visible parity uplift. After item 8 a
Finamp / Jellyfin-web session looks like real Jellyfin for direct play.
Items 9–15 = polish + admin.

## 6. Out of scope (✗)

Listed explicitly so reviewers push back on PRs sneaking them in.

- **Live TV** — 41 `/LiveTv/*` endpoints. Tuner discovery (HDHomeRun, IPTV M3U), EPG (Schedules Direct, XMLTV), DVR, series timers, listing-provider config. No tuner abstraction in Phase 1. Finamp / Infuse don't surface it anyway.
- **DLNA** — Removed upstream in 10.11 (zero `/Dlna/*` paths in snapshot). pharos follows; SSDP + content-directory XML not coming back. Cast / AirPlay (if ever wanted) are separate tasks post-parity.
- **Runtime plugin DLLs** — No stable Rust ABI; plugins are cargo crates composed at build (jellyfin-mapping.md §4). WASM host could restore dynamic extension; past Phase 2.
- **Broadcast / push channels** — `/Library/{Movies,Series,Media}/{Added,Updated}` inbound webhook for metadata agents. Scanner enumerates itself.
- **Admin web panel** — `/System/Configuration/*`, `/Dashboard/*`, `/Startup/*`, `/Environment/*`, plugin install, scheduled-task triggers, listing-provider config, `/Library/VirtualFolders` POST/DELETE, user create/delete, backup. Admin = TOML + CLI. Read-only stubs for pre-auth GETs; rest `✗`.
- **Per-item destructive ops** — `DELETE /Items`, `DELETE /Items/{itemId}`, `POST /Videos/MergeVersions`, `DELETE /Videos/{itemId}/AlternateSources`, metadata-editor POSTs, remote-search apply. Risk:reward bad — no client needs them.
- **Provider chrome** — `/Items/RemoteSearch/*`, `/Items/{itemId}/RemoteSearch/*`, `/Providers/{Subtitles,Lyrics}/*`, `/Tmdb/ClientConfiguration`. Behind plugin system. Phase 2.

## 7. Bookkeeping

Approximate bucket counts across the 315-path 10.11.10 surface:

- Implemented (`✓`): ~20 paths (System × 2, Users × 2, Items × 4, Videos/Audio × 4, Sessions × 8).
- Stub-priority (T19 §5 items 1–8): ~80 paths, mostly one-line empty / static.
- Real-impl backlog (T19 §5 items 9–15): ~60 paths.
- Won't-do (`✗`): ~140 paths — LiveTV, plugin/package, admin write, remote-search, dashboard, startup, backup, environment.
- Reserved for sibling tasks: 22 SyncPlay (T16/T17), 13 HLS + MediaInfo (T8/T9).

Re-derive via `curl https://api.jellyfin.org/openapi/jellyfin-openapi-stable.json | jq …`.

## References

- Jellyfin OpenAPI stable — https://api.jellyfin.org/openapi/jellyfin-openapi-stable.json
- jellyfin/jellyfin — https://github.com/jellyfin/jellyfin
- `docs/jellyfin-mapping.md` — architectural translation
- `docs/architecture.md` — component overview
- `SPEC.md` §V1, §V7 — invariants this audit protects
