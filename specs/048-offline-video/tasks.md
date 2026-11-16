# Tasks: Watch videos offline (048)

- [x] T1 Shared stream planner `pharos_hls::offline` (FR-002, FR-003).
- [x] T2 `KeptVideo` record, `fits`, `VideoStore` port (FR-003, ADR-0143).
- [ ] T3 Web store in Cache Storage, resume, persist (FR-002, FR-004).
- [ ] T4 Service worker answers `/videos/` from `pharos-videos-*` by exact URL (FR-005, ADR-0143).
- [ ] T5 Download control on the title page (FR-001).
- [ ] T6 Offline playback from the record; position saved locally (FR-005, FR-006).
- [ ] T7 Offline start lists downloads; Downloads page (FR-007, FR-008).
- [ ] T8 Phone app VideoStore over its files dir and asset proxy.
- [ ] T9 Re-negotiate a fresh play session when a download resumes after its session ended (6 h idle), keeping stored files of the same stream (PR #1482 review).
- [ ] T10 Serialise the download record list across tabs (Web Locks or BroadcastChannel) (PR #1482 review).
- [ ] T11 Bind progress and terminal updates to a run generation, so a removed-and-restarted download is not finished by the old run (PR #1482 review).
