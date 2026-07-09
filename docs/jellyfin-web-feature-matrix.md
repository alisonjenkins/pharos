# jellyfin-web feature matrix (test-oriented)

Structured inventory of the **jellyfin-web v10.11.8** (`cddd216da`) feature
surface, anchored to the real client source, used as the root for a
test-driven backlog. Each row maps a client feature → the server endpoint(s)
it calls → pharos status → the test that guards it.

- **status**: `DONE` (implemented, guarded by a live test), `THIN` (endpoint
  exists but drops/ignores data or returns a stub), `MISSING` (no endpoint).
- **Test**: the Rust test fn that asserts the behaviour. `#[ignore]` tests
  (marked *ignored*) are the backlog — enabling one and turning it green *is*
  the implementation task named by its `(Txx)` tag. `—` = no dedicated test
  yet / covered elsewhere.
- Test binaries live under `crates/pharos-server/tests/`:
  `jellyfin_feature_metadata.rs`, `jellyfin_feature_user_policy.rs`,
  `jellyfin_feature_library_options.rs`, `jellyfin_feature_inventory.rs`.
- Tests assert on the **Jellyfin wire JSON** (HTTP request/response), not
  pharos internal types — that is the contract jellyfin-web actually depends
  on, and it keeps the ignored tests compilable before the internal model
  grows the fields.

Companion docs (do not duplicate): `docs/jellyfin-parity-audit.md` (endpoint
audit), `docs/jellyfin-mapping.md` (concept mapping),
`docs/dioxus-parity-audit.md` (pharos's own UI parity — orthogonal).

Backlog task ids referenced below: **T66** (this scaffold), **T67**
(metadata richness), **T68** (user policy), **T69** (library options), **T70**
(playlists), **T72** (named-configuration persistence), **T73** (activity
log), **T74** (scheduled task execution), **T75** (plugins / packages),
**T76** (item ops: merge / content-type / remote images / remote subtitles /
lyrics / instant-mix). (T71 was reserved for DisplayPreferences, which turned
out already implemented — see below — so it is unused.)

---

## Onboarding / Startup wizard
jellyfin-web: `src/apps/wizard/routes/routes.tsx`, `src/controllers/wizard/*`.

| Feature | jellyfin-web source | Server endpoint | status | Test |
|---|---|---|---|---|
| Server name + UI language | `controllers/wizard/start/index.js` | `GET/POST Startup/Configuration`, `GET Localization/Options` | THIN | — |
| First admin user | `controllers/wizard/user/index.js` | `GET/POST Startup/User` | THIN | — |
| Library creation in setup | `controllers/wizard/library.js` | `POST Library/VirtualFolders` | THIN | `add_virtual_folder_persists_library_options` *(T69, ignored)* |
| Metadata language/country | `controllers/wizard/settings/index.js` | `POST Startup/Configuration`, `Localization/{Cultures,Countries}` | THIN | — |
| Remote access toggle | `controllers/wizard/remote/index.js` | `POST Startup/RemoteAccess` | THIN | — |
| Complete wizard | `controllers/wizard/finish/index.js` | `POST Startup/Complete` | THIN | — |

## Authentication / session
jellyfin-web: `src/controllers/session/*`, `src/components/quickConnect`.

| Feature | jellyfin-web source | Server endpoint | status | Test |
|---|---|---|---|---|
| Manual login (+ multi-user avatars) | `session/login/index.js` | `GET Users/Public`, `POST Users/AuthenticateByName` | DONE | `client_compat.rs::full_jellyfin_client_flow` |
| Quick Connect ladder | `apps/stable/routes/quickConnect/index.tsx` | `QuickConnect/{Enabled,Initiate,Authorize,Connect}` | DONE (T63) | `quick_connect_enabled_true` |
| Add / select server (multi-server) | `session/{addServer,selectServer}` | `GET System/Info/Public` | DONE | — |
| Forgot password + PIN | `session/resetPassword` | `POST Users/ForgotPassword[/Pin]` | MISSING | — |
| Change password | `user/userprofile.tsx` | `POST Users/{id}/Password` | DONE | — |
| Logout | connection manager | `POST Sessions/Logout` | DONE | — |
| Capability report on connect | `scripts/apploader` | `POST Sessions/Capabilities/Full` | DONE | — |

## Home screen
jellyfin-web: `src/controllers/home.js`, `src/components/homesections/`.

| Feature | jellyfin-web source | Server endpoint | status | Test |
|---|---|---|---|---|
| My Media tiles | `home.js` | `GET Users/{id}/Views` | DONE | — |
| Continue Watching / resume | `homesections/` | `GET Users/{id}/Items/Resume` | DONE | — |
| Next Up | `homesections/` | `GET Shows/NextUp` | DONE | — |
| Latest Media per library | `homesections/` | `GET Users/{id}/Items/Latest` | DONE | — |
| Live TV home rows | `homesections/` | `GET LiveTv/Programs/Recommended` | THIN | — |
| Home layout / section order | `components/homeScreenSettings/` | `GET/POST DisplayPreferences` | DONE | `display_preferences_roundtrip` |

## Libraries & views
jellyfin-web: `src/controllers/{movies,music,shows}/*`, `list.js`,
`src/components/{sortmenu,filterdialog,alphaPicker}/`.

| Feature | jellyfin-web source | Server endpoint | status | Test |
|---|---|---|---|---|
| Item grid/list (paged) | `list.js` | `GET Users/{id}/Items` (`ParentId`,`StartIndex`,`Limit`,`Fields`) | DONE | `jellyfin_items.rs` |
| Sorting | `sortmenu/` | `GET Items` (`SortBy`,`SortOrder`) | DONE | — |
| Filtering | `filterdialog/` | `GET Items/Filters`, `GET Items` (`Filters`) | DONE | — |
| Letter jump | `alphaPicker/` | `GET Items` (`NameStartsWith`) | DONE | — |
| Genres / Studios / Persons hubs | `moviegenres.js`, `tvstudios.js` | `GET Genres`, `GET Studios`, `GET Persons` | DONE | — |
| Music tabs (albums/artists/songs) | `music/*` | `GET Albums`, `GET Artists[/AlbumArtists]` | DONE | — |

## Item detail
jellyfin-web: `src/controllers/itemDetails/index.js`.

| Feature | jellyfin-web source | Server endpoint | status | Test |
|---|---|---|---|---|
| Core item load | `itemDetails/index.js` | `GET Users/{id}/Items/{id}` | DONE | — |
| Seasons / episodes | `itemDetails/index.js` | `GET Shows/{id}/{Seasons,Episodes}` | DONE | — |
| Similar items | `itemDetails/index.js` | `GET Items/{id}/Similar` | DONE | — |
| Special features / extras | `itemDetails/index.js` | `GET Users/{id}/Items/{id}/SpecialFeatures` | THIN (stub) | — |
| Instant mix (music) | `itemDetails/index.js` | `GET Items/{id}/InstantMix` | MISSING | `item_instant_mix` *(T76, ignored)* |

## Item metadata richness — **gap A**
The DTO carries every field; the **list** builder
(`pharos-jellyfin-api/src/dto.rs`) hardcodes people/studios/tags/external
arrays empty — only the single-item detail handler (`items.rs:get_item`)
enriches. `external_urls` / `remote_trailers` / `production_locations` are
never populated anywhere. `GET Items/{id}/MetadataEditor` is absent.

| Feature | jellyfin-web source | Server endpoint | status | Test |
|---|---|---|---|---|
| Cast & crew on lists | `cardBuilder`, `itemDetails` (`Fields=People`) | `GET Users/{id}/Items?Fields=People` | THIN | `list_items_populate_people_when_requested` *(T67, ignored)* |
| Studios & tags on lists | `Fields=Studios,Tags` | `GET Users/{id}/Items?Fields=Studios,Tags` | THIN | `list_items_populate_studios_and_tags` *(T67, ignored)* |
| Cast/crew on **detail** | `itemDetails` | `GET Users/{id}/Items/{id}` `People`/`Studios`/`Tags` | DONE | `item_detail_enriches_people_studios_tags` |
| Provider ids | `itemDetails`, external-link builder | `GET Items/{id}` `ProviderIds` | DONE | `list_and_detail_populate_provider_ids` |
| Chapters | `itemDetails` chapter strip | `GET Items/{id}` `Chapters` | DONE | — |
| External links | external-link builder (`ExternalUrls`) | `GET Items/{id}` `ExternalUrls` | MISSING | `item_external_urls_populated` *(T67, ignored)* |
| Remote trailers | `itemDetails` trailer button | `GET Items/{id}` `RemoteTrailers` | MISSING | `item_remote_trailers_populated` *(T67, ignored)* |
| Production locations | `itemDetails` | `GET Items/{id}` `ProductionLocations` | MISSING | `item_production_locations_populated` *(T67, ignored)* |
| Metadata-editor payload | `components/metadataEditor/` | `GET Items/{id}/MetadataEditor` (Cultures/ParentalRatings/ContentTypeOptions/ExternalIdInfos) | MISSING | `metadata_editor_endpoint_returns_cultures_and_external_ids` *(T67, ignored)* |

## Playback
jellyfin-web: `src/components/playback/playbackmanager.js`, `src/plugins/htmlVideoPlayer/`.

| Feature | jellyfin-web source | Server endpoint | status | Test |
|---|---|---|---|---|
| PlaybackInfo negotiation | `playbackmanager.js` | `POST Items/{id}/PlaybackInfo` (DeviceProfile) | DONE | `client_compat.rs` |
| HLS / direct stream | `htmlVideoPlayer/` | `GET Videos/{id}/master.m3u8`, `/stream` | DONE | `jellyfin_hls_*`, `jellyfin_stream` |
| Progress reporting | `playbackmanager.js` | `POST Sessions/Playing[/Progress,/Stopped]` | DONE | — |
| Trickplay scrubbing | `htmlVideoPlayer/` | `GET Videos/{id}/Trickplay/...` | DONE | `jellyfin_trickplay` |
| Skip intro/outro segments | `features/playback/utils/mediaSegments.ts` | `GET MediaSegments/{itemId}` | DONE (T64) | — |
| Intros / cinema mode | `playbackmanager.js` | `GET Users/{id}/Items/{id}/Intros` | THIN (stub) | — |

## Metadata editing
jellyfin-web: `src/components/{metadataEditor,itemidentifier,imageeditor,imageDownloader,refreshdialog}/`.

| Feature | jellyfin-web source | Server endpoint | status | Test |
|---|---|---|---|---|
| Edit item fields | `metadataEditor/` | `POST Items/{id}` | MISSING | `item_content_type` *(T76, ignored)* covers ContentType; edit-item tracked with T67 |
| Identify / remote search | `itemidentifier/` | `GET Items/{id}/ExternalIdInfos`, `POST Items/RemoteSearch/{type}`, `POST Items/RemoteSearch/Apply/{id}` | MISSING | *(T67 backlog)* |
| Refresh metadata | `refreshdialog/` | `POST Items/{id}/Refresh` | THIN | — |
| Image editor (delete/reorder) | `imageeditor/` | `GET Items/{id}/Images`, `DELETE .../{type}/{index}`, `POST .../{type}/{index}/Index` | MISSING | `remote_image_search` *(T76, ignored)* |
| Remote image search/download | `imageDownloader/` | `GET Items/{id}/RemoteImages`, `POST .../RemoteImages/Download` | MISSING | `remote_image_search` *(T76, ignored)* |

## Subtitles & lyrics
jellyfin-web: `src/components/{subtitleeditor,subtitleuploader,lyricseditor}/`.

| Feature | jellyfin-web source | Server endpoint | status | Test |
|---|---|---|---|---|
| Subtitle delivery | `htmlVideoPlayer/` | `GET Videos/{id}/{ms}/Subtitles/{idx}/Stream.{fmt}` | DONE | `jellyfin_subtitles` |
| Remote subtitle search/download | `subtitleeditor/` | `GET Items/{id}/RemoteSearch/Subtitles/{lang}`, `POST .../Subtitles/{id}` | MISSING | `remote_subtitle_search` *(T76, ignored)* |
| Subtitle upload/delete | `subtitleuploader/` | `POST/DELETE Videos/{id}/Subtitles` | MISSING | *(T76 backlog)* |
| Lyrics CRUD | `lyricseditor/`, `controllers/lyrics.js` | `GET/POST/DELETE …/Lyrics` | MISSING | `lyrics_crud` *(T76, ignored)* |

## Collections & playlists
jellyfin-web: `src/components/{collectionEditor,playlisteditor}/`.

| Feature | jellyfin-web source | Server endpoint | status | Test |
|---|---|---|---|---|
| Create / add-to collection | `collectionEditor/` | `POST Collections`, `POST/DELETE Collections/{id}/Items` | DONE | `jellyfin_collections` |
| Create / add / reorder playlist | `playlisteditor/` | `POST Playlists`, `POST Playlists/{id}/Items`, move/remove | MISSING (no controller) | `playlists_crud` *(T70, ignored)* |

## Search
jellyfin-web: `src/apps/stable/features/search/`.

| Feature | jellyfin-web source | Server endpoint | status | Test |
|---|---|---|---|---|
| Global hints | `useSearchItems.ts` | `GET Search/Hints` | DONE | `search_hints_returns_results` |
| Suggestions | `useSearchSuggestions.ts` | `GET Search/Suggestions` | DONE | — |

## User management / policy — **gap B**
Domain `pharos_core::UserPolicy` has one field (`admin: bool`);
`admin.rs:set_user_policy` builds `UserPolicy { admin: body.is_administrator }`
and drops every other key. `UserPolicyDto` serialises 16 flags but hardcodes
non-admin ones on read; the access/parental keys aren't in the DTO at all.

| Feature | jellyfin-web source | Server endpoint / policy key | status | Test |
|---|---|---|---|---|
| List / create / delete users | `users/index.tsx`, `add.tsx` | `GET Users`, `POST Users/New`, `DELETE Users/{id}` | DONE | — |
| Set admin flag | `users/profile.tsx` | `POST Users/{id}/Policy` `IsAdministrator` | DONE | `policy_roundtrip_is_administrator` |
| Disable user | `users/profile.tsx` | `IsDisabled` | THIN | `policy_roundtrip_is_disabled` *(T68, ignored)* |
| Hide user from login | `users/profile.tsx` | `IsHidden` | THIN | `policy_roundtrip_is_hidden` *(T68, ignored)* |
| Library access restriction | `users/access.tsx` | `EnableAllFolders`, `EnabledFolders` | MISSING | `policy_roundtrip_enabled_folders` *(T68, ignored)* |
| Parental control | `users/parentalcontrol.tsx` | `MaxParentalRating`, `BlockUnratedItems`, `AllowedTags`, `BlockedTags`, `AccessSchedules` | MISSING | `policy_roundtrip_parental` *(T68, ignored)* |
| Session limits | `users/profile.tsx` | `MaxActiveSessions`, `LoginAttemptsBeforeLockout`, `RemoteClientBitrateLimit` | MISSING | `policy_roundtrip_session_limits` *(T68, ignored)* |
| Feature flags | `users/profile.tsx` | `EnableRemoteAccess`, `EnableLiveTvAccess`, `EnableContentDownloading`, `EnableContentDeletion`, `SyncPlayAccess` | THIN | `policy_roundtrip_feature_flags` *(T68, ignored)* |
| Enforce: disabled → no login | (server behaviour) | `POST Users/AuthenticateByName` → 401 | MISSING | `enforce_disabled_user_cannot_authenticate` *(T68, ignored)* |
| Enforce: folder access filters items | (server behaviour) | `GET Users/{id}/Items` | MISSING | `enforce_enabled_folders_filters_items` *(T68, ignored)* |
| Enforce: parental rating filters items | (server behaviour) | `GET Users/{id}/Items` | MISSING | `enforce_max_parental_rating_filters_items` *(T68, ignored)* |
| Parental-ratings picker source | `users/parentalcontrol.tsx` | `GET Localization/ParentalRatings` | THIN (empty stub) | `localization_parental_ratings_nonempty` *(T68, ignored)* |

## Library management / options — **gap C**
`add_virtual_folder` (`items.rs`) deserialises only `PathInfos[].Path`;
`VirtualFolderOptionsDto` on read has just `EnablePhotos` +
`EnableRealtimeMonitor` (both hardcoded false). The
update/rename/paths endpoints and `GET Libraries/AvailableOptions` don't exist.

| Feature | jellyfin-web source | Server endpoint | status | Test |
|---|---|---|---|---|
| Add library | `mediaLibraryCreator/` | `POST Library/VirtualFolders` (`LibraryOptions`) | THIN | `add_virtual_folder_persists_library_options` *(T69, ignored)* |
| Remove library | `libraries/index.tsx` | `DELETE Library/VirtualFolders?name=` | DONE | `remove_virtual_folder_deletes` |
| Update library options | `mediaLibraryEditor/` | `POST Library/VirtualFolders/LibraryOptions` | MISSING | `update_virtual_folder_options_roundtrip` *(T69, ignored)* |
| Rename library | `mediaLibraryEditor/` | `POST Library/VirtualFolders/Name` | MISSING | `rename_virtual_folder` *(T69, ignored)* |
| Add / remove media path | `mediaLibraryEditor/` | `POST/DELETE Library/VirtualFolders/Paths` | MISSING | `add_and_remove_media_path` *(T69, ignored)* |
| Available options (fetchers/TypeOptions) | `libraryoptionseditor/` | `GET Libraries/AvailableOptions` | MISSING | `available_options_lists_fetchers_and_typeoptions` *(T69, ignored)* |
| Folder picker | `directorybrowser/` | `GET Environment/DirectoryContents` | THIN | `environment_directory_contents` *(T69, ignored)* |

## Dashboard settings (named configurations)
jellyfin-web: `src/apps/dashboard/routes/{settings,playback,libraries,livetv}/*`.
Each page reads `GET System/Configuration/{key}` and writes
`POST System/Configuration/{key}`. pharos accepts POST (no-op) but has **no GET**.

| Config key | jellyfin-web page | status | Test |
|---|---|---|---|
| general (`System/Configuration`) | `settings/index.tsx` | DONE (T65 branding subset) | — |
| `encoding` | `playback/transcoding.tsx` | THIN (GET serves defaults; POST no-op) | `named_configuration_encoding` *(T72, ignored)* |
| `network` | `controllers/networking.js` | THIN (GET defaults; POST no-op) | `named_configuration_network` *(T72, ignored)* |
| `metadata` | `libraries/display.tsx` | THIN (GET empty; POST no-op) | `named_configuration_metadata` *(T72, ignored)* |
| `livetv` | `livetv/index.tsx` | THIN (GET defaults; POST no-op) | `named_configuration_livetv` *(T72, ignored)* |
| `xbmcmetadata` (NFO) | `libraries/nfo.tsx` | THIN | *(T72 backlog)* |
| `branding` | `branding/index.tsx` | DONE (T65) | — |

## Other admin surfaces

| Feature | jellyfin-web source | Server endpoint | status | Test |
|---|---|---|---|---|
| API keys | `keys/index.tsx` | `GET/POST/DELETE Auth/Keys` | DONE (T58) | `api_keys_endpoint_present` |
| Scheduled tasks (list) | `tasks/index.tsx` | `GET ScheduledTasks` | THIN (empty) | — |
| Scheduled tasks (run/triggers) | `tasks/task.tsx` | `POST ScheduledTasks/{id}/Triggers`, start/stop | MISSING | `scheduled_task_execution` *(T74, ignored)* |
| Plugins (list) | `plugins/index.tsx` | `GET Plugins` | THIN (empty) | — |
| Plugin install / packages | `plugins/plugin.tsx` | `GET Packages`, `POST Packages/Installed/{name}` | MISSING | `plugins_install` *(T75, ignored)* |
| Activity log | `activity/index.tsx` | `GET System/ActivityLog/Entries` | THIN (empty stub) | `activity_log_entries` *(T73, ignored)* |
| Logs | `logs/index.tsx` | `GET System/Logs[/Log]` | DONE (T62) | — |
| Devices | `devices/index.tsx` | `GET Devices`, `POST Devices/Options`, `DELETE Devices` | THIN | — |
| Branding (runtime) | `branding/index.tsx` | `GET Branding/Configuration`, `POST System/Configuration` | DONE (T65) | — |
| Backups | `backups/index.tsx` | `GET/POST Backup*` | THIN | — |
| Notifications | `dashboard/notifications` | `GET Notifications/{Types,Services}` | THIN (stub) | — |

## Live TV

| Feature | jellyfin-web source | Server endpoint | status | Test |
|---|---|---|---|---|
| Guide / channels | `livetvguide.js` | `GET LiveTv/{Programs,Channels,GuideInfo}` | THIN | — |
| Recordings | `livetvrecordings.js` | `GET LiveTv/Recordings` | THIN | — |
| Timers / series timers | `livetvschedule.js` | `GET/POST/DELETE LiveTv/{Timers,SeriesTimers}` | THIN | — |
| Tuners / listing providers | `controllers/livetvtuner.js` | `GET/POST LiveTv/{TunerHosts,ListingProviders}` | THIN | — |

## Item context / bulk actions

| Feature | jellyfin-web source | Server endpoint | status | Test |
|---|---|---|---|---|
| Favorite toggle | `userdatabuttons/` | `POST/DELETE UserFavoriteItems/{id}` | DONE | `favorite_toggle_roundtrip` |
| Played / unplayed toggle | `multiSelect/`, `userdatabuttons/` | `POST/DELETE UserPlayedItems/{id}` | DONE | `played_toggle_roundtrip` |
| Rating / likes | `userdatabuttons/` | `POST UserItems/{id}/Rating` | THIN | — |
| Merge versions | `multiSelect/` | `POST Videos/MergeVersions` | MISSING | `item_merge_versions` *(T76, ignored)* |
| Set content type | `metadataEditor/` | `POST Items/{id}/ContentType` | MISSING | `item_content_type` *(T76, ignored)* |
| Delete item | `multiSelect/` | `DELETE Items/{id}` | THIN | — |

## SyncPlay / remote control

| Feature | jellyfin-web source | Server endpoint | status | Test |
|---|---|---|---|---|
| Groups (list/new/join/leave) | `components/syncPlay/` | `SyncPlay/{List,New,Join,Leave}` | DONE | `syncplay.spec.ts` |
| Playback commands | `components/syncPlay/` | `SyncPlay/{Play,Pause,Seek,Ready,Buffering}` | DONE | — |
| Sessions list | `remotecontrol/` | `GET Sessions` | DONE | — |
| Remote play/state/general commands | `remotecontrol/` | `POST Sessions/{id}/{Playing,Command,Message}` | DONE (T40/T60) | — |

## DisplayPreferences (user settings persistence)

| Feature | jellyfin-web source | Server endpoint | status | Test |
|---|---|---|---|---|
| View mode / sort / home layout / subtitle appearance | `components/{displaySettings,subtitlesettings,homeScreenSettings}/` | `GET/POST DisplayPreferences` | DONE (persists per user/id/client) | `display_preferences_roundtrip` |
| Playback / display / subtitle prefs (UserConfiguration) | `components/playbackSettings/` | `POST Users/{id}/Configuration` | DONE (T55) | — |
