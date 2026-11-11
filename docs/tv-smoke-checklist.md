# TV app: checks before every install

The TV app's fixes have come back after they were made, because nothing
re-checked them (B1329, B1332, B1333). Most are now automated; this list
is what each build still needs, in order. Run it for every APK before it
is installed on the TV, and record the build's result in its thread.

## 1. Automated, no TV needed

| Check | Command | Guards |
|---|---|---|
| The TV build type-checks | `just tv-check` | Android-only code (B1333, V1304) |
| Manifest and launcher resources | `cargo nextest run -p pharos-ui -E 'test(the_tv_)'` | label, icon, banner, dark theme, config changes (V1302, V1294) |
| The remote, off the TV | `just compat-dioxus-full on` (runs `tv-remote.spec.ts`) | seek bar stop, seek preview, album tiles, search OK (V1303) |
| The APK itself | `just tv-apk-check <apk>` | the built APK has the overlay, versionCode and config changes (V1302) |

Build only with `nix run .#android-apk`. A `dx build` run by hand skips
the resource overlay and the versionCode, and its APK fails
`tv-apk-check`: it shows dx's robot icon and a white launch on the TV.

## 2. On the TV

Before any of this: the TV's volume is 0, read back. Never attach a
virtual keyboard or other input device to the TV: it recreates the
activity (B1323). Use the remote, or `adb shell input keyevent`.

| # | Do | Expect | Log line (`adb logcat -s pharos`) |
|---|---|---|---|
| 1 | Cold start from the launcher | Pharos icon and banner; dark from the first frame, no white | `pharos: bridge installed` |
| 2 | Home, Down | the focus is on a tile, ringed | |
| 3 | Open a film, Down, Up | Down: control row; Up: the seek bar takes the focus | |
| 4 | On the seek bar, Right x3, OK | the preview moves and stays whole on screen at either end; OK seeks, focus stays on the bar | `pharos: scrub settled input=remote` |
| 5 | Turn subtitles on in a wide (letterboxed) film | the bottom line is whole, above the screen edge | `pharos: picture crop … rows_kept=cues` |
| 6 | Music, an artist, Down, OK | the album page opens; nothing is queued | |
| 7 | Search, type, Down, OK | the match opens; the keyboard does not come back | `pharos: remote ok opened match` |
| 8 | A library, pick a genre, open a title, Back | the same filtered grid, the opened tile focused | `pharos: grid place restored placed=true` |
| 9 | Back from Home | the app asks or stays; it never closes on a stray Back | |

A row that fails is a regression: say which row, with its log lines, in
the build's thread before the APK is handed over.
