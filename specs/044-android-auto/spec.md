# 044 — Phone app with Android Auto (pure Rust)

- **Status:** draft. Slice 0 (spike) done 2026-11-14, `spike-0.md`. Slice 1
  (real content, artwork, streaming) in progress.
- **Date:** 2026-11-14
- **Asked by:** Alison, "What would be required for us to make a phone app
  and have it integrate with android auto?" (07:35Z); "FYI we want music,
  audio books and if possible voice control via gemini" (07:42Z); "We also
  need to get it showing real content complete with album artwork" (08:35Z).
- **Ids:** ADR-0127 (Accepted). B/V allocated when slices land.
- **Depends on:** 017 (native API: `deviceToken`, `quickConnect*`, media
  token ADR-0058), 025 (Android build, signing, native websocket client),
  004 (books; audiobooks are being added there).

## Goal

Music and audiobooks from Pharos play in Android Auto, browsed on the car
screen and asked for by voice through Gemini, from an Android phone app
written only in Rust. No Java or Kotlin source exists: the classes Android
instantiates are generated from Rust (ADR-0127).

## How it runs

Android Auto binds the app's media browser service and draws its own
template UI from what the service returns; the app's own UI never shows in
the car. Video, lyrics and karaoke are not offered: Android Auto does not
allow them while driving.

| Android entry point | Generated class | Rust does |
|---|---|---|
| Media browser service (manifest, `MediaBrowserService` filter) | `PharosMediaService` | session + token on create; browse tree; search |
| Session callback | `PharosSessionCallback` | play / pause / stop / next / previous / seek / play-from-id / play-from-search |
| Voice from the phone assistant (`MEDIA_PLAY_FROM_SEARCH`) | `PharosSearchActivity` (no UI) | reads `query`, plays the best match, finishes |
| Artwork (`content://` icon URIs) | content provider (slice 1) | serves cached cover images |

Playback uses `android.media.MediaPlayer` from Rust over JNI. The APK is
built without Gradle (`spikes/044-android-auto/build-apk.sh`): Rust class
generator → `d8` → `aapt2` → zip → `zipalign` → `apksigner` (the TV dev key).

## User stories

### US1: browse and play music in the car (P1)

In Android Auto the user opens Pharos, sees Music (recently played, albums,
artists) with album artwork, picks an album and plays it. Car controls and
steering-wheel buttons pause, skip and seek.

### US2: audiobooks (P1)

Audiobooks shows the user's books with audio, resuming where they left off.
Depends on spec 004's audiobook work.

### US3: voice (P2)

"Play <album / artist / book> on <app name>" via Gemini starts it. With
Pharos already playing, "play <x>" alone goes to Pharos.

### US4: sign in (P1)

First run in the car shows a browse item "Sign in: approve code NNNNNN". The
user approves it in the Pharos web UI (QuickConnect, as the TV app does);
the phone keeps the device token.

### US5: offline (P3, later)

Download albums and books to the phone for no-signal stretches.

## Slices

0. **Spike** (done): generated classes, fixed test catalogue, tones play in
   the Desktop Head Unit on a real phone. `spike-0.md`.
1. **Real content:** QuickConnect sign-in, library from the native API,
   artwork through a content provider, HTTP streaming with `prepareAsync`.
   Lanes run in parallel: content module (Rust, no JNI); artwork + streaming
   (JNI); device testing.
2. **Product app:** move out of `spikes/` into a workspace crate, foreground
   media notification, audio focus, resume positions, the app's spoken name
   (see Open questions), phone UI (reuse the Dioxus Android build from 025
   for the phone screen).
3. **Offline downloads.**

## Distribution

Sideloaded APK; Android Auto lists it because Android Auto's developer
settings have "Unknown sources" on. Play Store (internal track) stays an
option: about 2–3 days plus review.

## Open questions

- **Spoken name.** Gemini transcribes "Pharos" as "Pharaoh's" and falls back
  to the default music app. Options: a plain label, an alias, or relying on
  "play X" while Pharos is the active media app. See `spike-0.md` §Voice.
