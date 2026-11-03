# Tasks: image subtitles drawn by the native player

- [x] T1 Signal: `subtitle.delivery` shows `External` for a drawn image codec; `pharos_subtitle_image_extract_total{outcome}` registered and asserted.
- [x] T2 Fixture: `dvdSubtitleMkv` in `flake.nix` (testsrc + tone + `dvd_subtitle` from `spumux` PNG events), added to the test media.
- [x] T3 Decode: `libav/subtitle_images.rs` events + RGBA PNGs, end-time rule (FR-005); unit test on the fixture.
- [x] T4 Worker op + pool method.
- [x] T5 Cache: `SubtitleCache::image_subtitles` (layout, versioned directory, in-flight dedupe, progressive manifest). The byte cap is a known gap (plan.md).
- [x] T6 Routes: manifest + PNG, auth, 404 for not-yet-extracted.
- [x] T7 Negotiation + native API: `imageSubtitles` caps, `image @2`, `write_tracks`, client mapping; tests that a declaring client is not downgraded and a non-declaring one still burns.
- [x] T8 Player: declare codecs, overlay layer, manifest polling, in-place switching.
- [x] T9 e2e: pick the DVD subtitle, same play session, overlay `<img>` drawn; disarm check.
- [x] T10 Spec bookkeeping: V/B ids, ADR 0058 row, 013 FR-003 pointer.
