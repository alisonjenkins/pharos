# Dioxus UI parity audit (T52)

Snapshot of pharos Dioxus surface vs jellyfin-web (10.11.x). Status legend: ✅ have, 🟡 partial, ⛔ missing. SPEC §T cite links the follow-up row.

## Auth + session

| jellyfin-web route | status | notes | cite |
|---|---|---|---|
| `/#/login` | ✅ | LoginForm + RootApp gate | T25 |
| `/#/selectserver` | ⛔ | Single-server only via `window.location.origin` | T59 |
| QuickConnect ladder | ⛔ | No /QuickConnect endpoints emitted server-side either | T59 |
| Forgot Password | ⛔ | No mail flow planned | T59 |
| Multi-server credential storage | ⛔ | localStorage strategy TBD | T59 |

## Library browse

| jellyfin-web route | status | notes | cite |
|---|---|---|---|
| `/#/home` recent / latest hubs | ⛔ | LibraryView is flat list, no hub rows | T58 (home prefs) |
| `/#/mymedia` library picker | 🟡 | LibraryView lists items; no per-library tiles | (in T58 libraries pane) |
| `/#/list?type=Movie` movies grid | 🟡 | LibraryView shows all kinds; no filter UI | T54 phase 2 / future |
| `/#/list?type=Series` shows grid | 🟡 | same | future |
| `/#/list?type=MusicAlbum` albums | ⛔ | No album grouping yet | future |
| `/#/list?type=BoxSet` collections | ⛔ |  | future |
| `/#/list?type=Playlist` playlists | ⛔ |  | future |
| `/#/livetv.html` Live TV | ⛔ | server endpoints land in T47 | T56 |
| `/#/search` global search | ✅ | SearchView via /Search/Hints | T53 |

## Item detail

| jellyfin-web route | status | notes | cite |
|---|---|---|---|
| `/#/details?id=…` movie | 🟡 | ItemDetailView phase 1 — title + runtime + actions; no images, no cast | T54 |
| `/#/details?id=…` episode | 🟡 | shows runtime; no S/E render, no series breadcrumb | T54 phase 2 |
| `/#/details?id=…` album | 🟡 | no track list, no artist link | T54 phase 2 |
| `/#/details?id=…` artist | ⛔ | no Artist view | future |
| Cast + crew sidebar | ⛔ | server emits People — unused client-side | T54 phase 2 |
| Backdrop / primary image | ⛔ | server emits Images endpoints (T34) — unused client-side | T54 phase 2 |
| Chapter markers + thumbnails | ⛔ | server emits trickplay map; no UI | future |
| Genre tag links | ⛔ |  | future |

## Playback

| jellyfin-web route | status | notes | cite |
|---|---|---|---|
| Native `<video>` direct play | ✅ | PlayerView | T26 |
| HLS adaptive bitrate | ⛔ | server emits master.m3u8 (T9); UI uses direct stream URL | T57 |
| Subtitle picker | 🟡 | Native `<track>` + side picker (T57 phase 1) | T57 |
| Audio track picker | 🟡 | Side picker only — does not switch streams | T57 |
| Quality picker (DeviceProfile MaxStreamingBitrate) | ⛔ |  | T57 |
| Captions toggle | 🟡 | Browser-default CC button only | T57 |
| Scrub bar w/ chapter marks | ⛔ | Browser scrub only, no chapter overlay | T57 |
| Fullscreen | ⛔ | Browser-default `requestFullscreen` not wired | T57 |
| Audio-only minimised view | ⛔ |  | T57 |
| Skip intro / outro | ⛔ |  | future |

## Group / SyncPlay

| jellyfin-web feature | status | notes | cite |
|---|---|---|---|
| Create + join group | ✅ | GroupSessionPanel | T27 |
| Leader badge + buffering | ✅ | GroupSessionPanel | T27 |
| Chat | ⛔ | T27 phase 2 |
| Position drift indicator | ⛔ | T49 phase 2 |

## Preferences (`/#/mypreferences*`)

| pane | status | cite |
|---|---|---|
| Display | ⛔ | T55 |
| Playback | ⛔ | T55 |
| Subtitles | ⛔ | T55 |
| Home | ⛔ | T55 |
| Languages | ⛔ | T55 |
| Quick Connect | ⛔ | T59 |

## Dashboard (`/#/dashboard.html`)

| pane | status | notes | cite |
|---|---|---|---|
| Users | 🟡 | AdminView (T50) list + create + delete; no policy editor or password reset | T58 (T50 follow-up) |
| Libraries | ⛔ | server `/Library/VirtualFolders` exists; no CRUD | T58 |
| Scheduled tasks | ⛔ | server returns empty stub | T58 |
| Plugins | ⛔ | server returns empty stub | T58 |
| Logs | ⛔ | server returns empty stub | T58 |
| Metadata + image providers | ⛔ |  | T58 |
| ffmpeg | ⛔ | config-only on server today | T58 |
| Networking | ⛔ |  | T58 |
| Branding | ⛔ |  | T58 |
| DLNA | ⛔ | T48 ships protocol; no UI | T58 |
| Notifications | ⛔ |  | T58 |
| API keys | ⛔ | no `/Auth/Keys` endpoint yet | T58 |
| Devices | ⛔ | /Sessions has device list; no UI | T58 |
| Activity log | ⛔ | stubbed empty | T58 |

## Casting + remote control

| feature | status | cite |
|---|---|---|
| Cast bar (popout target picker) | ⛔ | T60 |
| Remote PlayState commands | ⛔ | T60 + T40 phase 2 |
| Volume control | ⛔ | T60 |

## Misc

| jellyfin-web feature | status | cite |
|---|---|---|
| Help dialogs | ⛔ | low priority |
| Downloads / offline | ⛔ | future |
| Cast to Chromecast | ⛔ | future |
| Locale / i18n picker | ⛔ | T55 (languages) |

## Coverage summary

Counted ~80 jellyfin-web feature units above. pharos: 5 ✅, 12 🟡, ~63 ⛔ — roughly **15%** of jellyfin-web's surface reachable today, up from the ~5% estimate in T52's description. Follow-up rows T54–T60 are the actual roadmap.
