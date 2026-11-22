# Tasks: Keep the normal UI offline (049)

- [x] T1 `OfflineLibrary` answers `PharosApi` reads from the catalogue and what is on the device (FR-001–FR-003, V1483, V1488, V1489).
- [x] T2 `device_verdict` and the app's switch, with the offline and back-online lines (FR-005, FR-006, V1484).
- [x] T3 Screens read through `Ctx::library` (FR-001).
- [x] T4 Notes at sign-in, every 5 min, daily refresh, prune, forget on sign-out (FR-007, V1485).
- [x] T5 Title page offers only what works offline (FR-004, V1486).
- [x] T6 Browser posters in `pharos-art-<user>`, served by the worker (FR-008, V1487).
- [ ] T7 Browser end-to-end: offline cold start, a dropped session, and back online (Playwright `setOffline`).
- [ ] T8 Phone: replace the offline start screen with the normal screens; check cached songs play from the album page offline (FR-010).
- [ ] T9 Kobo: same offline experience (FR-011, V1490).
