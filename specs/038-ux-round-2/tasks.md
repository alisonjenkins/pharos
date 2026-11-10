# Tasks — spec 038 UX round 2

One group per thread. Each group's first task reproduces the problem and names
or adds its signal (ODD step 1), as its own commit. `[ALISON]` tasks wait for
her answer. `[P]` tasks touch disjoint files and can run in parallel.

Id blocks (bugs and invariants are written with the fix):

| Group | B ids | V ids |
|---|---|---|
| G1 Home rails and resume rules | B1224–B1232 | V1187–V1195 |
| G2 Show page and browse | B1233–B1244 | V1196–V1207 |
| G3 Player controls by touch | B1245–B1252 | V1208–V1215 |
| G4 Player feedback | B1253–B1260 | V1216–V1223 |
| G5 Watching together feedback | B1261–B1266 | V1224–V1229 |
| G6 TV with a remote | B1267–B1277 | V1230–V1240 |
| G7 Admin and settings | B1278–B1283 | V1241–V1246 |

## G1 — Home rails and resume rules (server)

- [x] T038-100 Reproduce on a seeded library: an unstarted show in Next Up, a started one missing, one new season filling a latest rail, a 5 s play in Continue Watching. Record which existing signal shows each, if any.
- [x] T038-101 Add `pharos_next_up_total{outcome}` and `pharos_play_stop_total{outcome,reason}`; assert both in tests and watch them fail when disarmed.
- [x] T038-102 Tests pinning the Next Up rule: started series only, episode after the last played, season 0 rule, order by last play, nothing repeated from Continue Watching (FR-101–FR-104).
- [x] T038-103 Next Up driven from the viewer's `user_data` rows (FR-105); run `just test-postgres`.
- [x] T038-104 ADR: the Next Up rule.
- [x] T038-105 Latest rails grouped by series and album with a new-children count (FR-106); schema change if the count travels on the wire (get the schema number from the merge queue).
- [ ] T038-106 [ALISON] Minimum resume point setting and rule (FR-107), with its ADR.
- [x] T038-107 Stop path reports its failures (FR-108).
- [ ] T038-108 [ALISON] Resume tiles play at once (FR-109).

## G2 — Show page and browse

- [ ] T038-200 Reproduce each G2 problem in a browser (desktop and 390px), keep the screenshots as the before state, and name or add the signal that shows each one.
- [ ] T038-201 Show page opens on the season being watched; hero Play is the show's next-up (FR-201).
- [ ] T038-202 Reproduce the phone episode row at 390px, then stack it (FR-202).
- [ ] T038-203 Music library letter rail and album sort (FR-203); tab in the URL (FR-204).
- [ ] T038-204 Series page loads in parallel (FR-205).
- [ ] T038-205 Home and title pages keep content and scroll on Back (FR-206).
- [ ] T038-206 Search URL and grouped "see all" (FR-207).
- [ ] T038-207 [P] Filter selects by arrow keys on web (FR-208); Manage page in the URL (FR-209).
- [ ] T038-208 Empty, failed and busy states (FR-210); confirms and palette places (FR-211).
- [ ] T038-209 Hero art at drawn size (FR-212); palette viewport and focus ring, What's New keys and save message (FR-213).

## G3 — Player controls by touch and pointer

- [x] T038-300 Reproduce on a touch device profile at 390x844: seek bar ignores touch, control row height, target sizes. Keep the before screenshots and name or add the signal that shows each one.
- [x] T038-301 Seek bar: drag, touch, buffered range, focus (FR-301), with a group test for one command per scrub.
- [x] T038-302 Narrow-screen control bar and settings sheet (FR-302); screenshots at 390x844 for Alison before merge.
- [x] T038-303 44px targets and double-tap zones (FR-303); wheel rule (FR-304).
- [x] T038-304 SVG glyphs (FR-305); small-player seek (FR-306).
- [x] T038-305 Speed control and subtitle delay (FR-307).
- [x] T038-306 [ALISON] Key map for `k`/`j` (FR-308). Declined by Alison 2026-11-10: `k`/`j` stay as volume, no change.

## G4 — Player feedback

- [x] T038-400 Reproduce: a denial with no trace id, one caption for every wait (include a saturated encoder), Up Next over the ending without detected credits. Name the query that shows each.
- [x] T038-401 Trace id on every denial, shown in error panels (FR-404); verify by query.
- [x] T038-402 Server load hint for a queued play or seek (FR-402).
- [x] T038-403 Wait caption by cause (FR-401), using T038-402.
- [x] T038-404 [P] Failure panel hint by kind and Report a problem (FR-403); Back during first load (FR-408).
- [x] T038-405 [P] Up Next pill without detected credits (FR-405); Up Next in the small player (FR-406).
- [x] T038-406 Refused autoplay (FR-407); confirm with the browser video regression thread first.

## G5 — Watching together feedback

- [x] T038-501 Check whether a group session rebinds after a reconnect; fix if not (FR-503). It does (B894); its outcome is now reported (B1261).
- [x] T038-502 [P] Rejoin note (FR-501); waiting pill (FR-502); connection banner in the player (FR-503). B1262–B1264.
- [ ] T038-503 Together page (FR-504); Leave undo (FR-505). Done but for the page refreshing from server push, which follows with schema 41 (B1265).

## G6 — TV with a remote

- [ ] T038-600 Reproduce each G6 problem on the TV with a remote (volume 0, read back first) and name or add the signal that shows it; where none exists, T038-602 adds it before any fix.
- [ ] T038-601 ADR and plan for landing the TV playback line on main; CI compiles the `mobile` feature (FR-601). Needs the merge queue's decision on the held TV PR.
- [ ] T038-602 TV telemetry (FR-608) first, so the rest can be seen.
- [ ] T038-603 Search from the banner (FR-602).
- [ ] T038-604 Return to the opened tile (FR-603).
- [ ] T038-605 OK on Skip and Up Next; Back order (FR-604, FR-607).
- [ ] T038-606 Report and pause on leaving the app (FR-605).
- [ ] T038-607 [P] Inputs by D-pad (FR-606); transport keys (FR-609); out-of-date notice (FR-610).
- [ ] T038-608 Test each on the TV with Alison (volume 0, read back first).

## G7 — Admin and settings

- [x] T038-700 Reproduce: a scan with no progress in the UI while the Task topic is published; check whether a scan outcome metric already exists.
- [x] T038-701 Scan progress, result and per-library scan from the Task topic (FR-701).
- [x] T038-702 Preferred audio and subtitle language (FR-702).
- [x] T038-703 [P] Sign-out and Remove confirms for kept books (FR-703); coordinate with the ebook work.
- [x] T038-704 [P] Music queue survives reload (FR-704); visualiser idle while paused (FR-706).
- [x] T038-705 [ALISON] Storage notice on the listen view (FR-705).
