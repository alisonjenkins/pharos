# Tasks: audiobooks (046)

**Input**: [spec.md](spec.md), [plan.md](plan.md), ADR-0132. Ids are local to this spec (`T046-NN`). No real titles, authors, narrators or paths in code, tests or fixtures.

## Slice 1 - Scanner

- [x] T046-01 `audiobook.rs`: `classify`, `folder_tracks`, `apply`, labels with a distinctness test (FR-002 to FR-004).
- [x] T046-02 Walk `.m4b`; `probe_one` classifies, applies or skips, logs and counts (FR-001, FR-005). Disarm the counter once and watch the scan test fail.

## Slice 2 - Listing

- [x] T046-03 Album DTOs carry their length and the audiobook mark; the native summary carries `audiobook`, author and length (FR-007).
- [x] T046-04 `it_rpc::library::a_books_library_lists_its_audiobooks_as_albums`: books default keeps books only, albums are marked, a music album is not (FR-006, FR-007). Disarm the mark once.
- [x] T046-05 UI: Audiobooks tab, tabs shown only when they have items, tile author and length (FR-008).

## Slice 3 - Playback

- [x] T046-06 An audiobook's track finishes only in its last minute (FR-010, B1430). Disarm once.
- [x] T046-07 The album page resumes on the part last begun (FR-009). Disarm once.
- [x] T046-08 `AudiobookControls`: speed, sleep timer, chapters (FR-011).

## Open

- [ ] T046-09 Read a narrator tag into the probe (needs a column or a JSON field).
- [ ] T046-10 Key albums by folder so books of one title stay apart.
- [ ] T046-11 Renumber a folder book's later tracks when the watcher adds a file between them.
- [ ] T046-12 Verify on a deployed server: `sum by (verdict, reason) (increase(pharos_scan_audiobook_total[1d]))` after the first real audiobook lands.
