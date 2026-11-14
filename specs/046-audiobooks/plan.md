# Implementation plan: audiobooks (046)

**Spec**: [spec.md](spec.md). **ADR**: ADR-0132.

## Shape

| Place | Holds |
|---|---|
| `pharos-scanner/src/audiobook.rs` | `classify` (pure but for the folder listing it is handed), `folder_tracks` (natural order, `book::comic::natural_cmp`), `BookTrack::apply` onto the probe, the `Verdict` / `Reason` labels and `record`. |
| `pharos-scanner/src/fs.rs` | `.m4b` in `DEFAULT_EXTENSIONS`; `probe_one` classifies audio under a Books root off the reactor and applies the book, or skips the file. |
| `pharos-server/src/state.rs` | `AppState::is_audiobook`. |
| `pharos-server/src/api/jellyfin/items.rs` | Album DTOs carry `RunTimeTicks` and, for an audiobook, `PharosAudiobook`. |
| `pharos-server/src/api/pharos/rpc/library.rs` | `SummaryFacts` carries the library roots; summaries set `audiobook`, an audiobook album's `artist` and `runtimeMs`. |
| `pharos-server/src/services/play_state.rs` | `finished` with the audiobook slack. |
| `pharos-api` schema | `ItemSummary.audiobook @18`, schemaVersion 48. |
| `pharos-client` | `Item::audiobook`. |
| `pharos-ui/src/native/browse.rs` | `BooksTab::Audiobooks`, `BooksTab::shown`, the album listing on load, the Audiobooks grid, the album page's Resume. |
| `pharos-ui/src/native/views.rs` | An audiobook tile's author and length line. |
| `pharos-ui/src/native/audiobook.rs` | `AudiobookControls`: speed, sleep timer, chapters; `resume_track`. |
| `pharos-ui/src/native/listen.rs`, `app.rs` | The `audiobook` prop and where it is built. |

No migration: "is an audiobook" is derived from the library roots, and resume uses the per-item position.

## Signals

- Scanner decision: `sum by (verdict, reason) (rate(pharos_scan_audiobook_total[1h]))`.
- Loki: `{namespace="pharos"} |= "scanner: audio in a books library classified"`.
- Stop verdicts: `pharos_play_stop_total{outcome}` as before; the debug line "playback stop: finished or kept" carries `audiobook`.
- UI: `pharos: audiobook speed`, `pharos: audiobook sleep set`, `pharos: audiobook sleep fired`, `pharos: audiobook chapters unavailable`.

## Gates

- Clippy deny list (V17): no `unwrap` / `expect` outside tests.
- The scanner counter test disarmed once and watched fail.
- No sqlx query touched, so no postgres arm is needed.
- wasm check of `pharos-ui-web`.
