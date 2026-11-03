# Tasks: image subtitles drawn by the native player

- [ ] T1 Signal: `subtitle.delivery` shows `Image`; `pharos_subtitle_image_extract_total{outcome}` registered and asserted.
- [ ] T2 Fixture: `dvdSubtitleMkv` in `flake.nix` (testsrc + tone + `dvd_subtitle` from `spumux` PNG events), added to the test media.
- [ ] T3 Decode: `libav/subtitle_images.rs` events + RGBA PNGs, end-time rule (FR-005); unit test on the fixture.
- [ ] T4 Worker op + pool method.
- [ ] T5 Cache: `subtitle_image_cache` (layout, version wipe, cap, in-flight dedupe, progressive manifest).
- [ ] T6 Routes: manifest + PNG, auth, 404 for not-yet-extracted.
- [ ] T7 Negotiation + native API: `imageSubtitles` caps, `image @2`, `write_tracks`, client mapping; tests that a declaring client is not downgraded and a non-declaring one still burns.
- [ ] T8 Player: declare codecs, overlay layer, manifest polling, in-place switching.
- [ ] T9 e2e: pick the DVD subtitle, same play session, overlay `<img>` drawn; disarm check.
- [ ] T10 Spec bookkeeping: V/B ids, ADR 0058 row, 013 FR-003 pointer.
