# Tasks: Pharos-styled animated subtitles (028)

**Input**: [spec.md](spec.md), [plan.md](plan.md), ADR-0067. Ids are local to this spec (`T028-NN`), ordered by slice. `[P]` = can run in parallel with the other `[P]` tasks of its slice.

Every task follows TDD: a failing test first, then the code. Metrics and log lines are asserted, with a disarm check.

## Slice 1 — plan, theme, compiler, effects (B601; V532, V533)

- [ ] T028-01 Create `crates/pharos-styling` (workspace member, `[lints] workspace = true`, run `just hakari-regen`). Add `plan.rs` (EffectPlan, LineEffect, Intent with 11 values and `label()`, Placement, Karaoke, Tier, EffectsLevel), `version.rs`, and serde for the plan. Test: the labels are distinct and the plan round-trips through JSON.
- [ ] T028-02 `source.rs`: read SRT, WebVTT and ASS into `SourceLine { start_ms, end_ms, text, raw, style, italic, typeset, karaoke: Option<Vec<Syllable>> }`. `typeset` means the event carries `\pos`, `\move`, `\an` other than 2, `\clip` or `\p`. Tests: one fixture per format, `<i>` and `{\i1}` italics, a `\k` line, and a sign line.
- [ ] T028-03 `budget.rs`: an override-tag scanner and `check(ass, tier) -> Result<(), Vec<BudgetBreach>>`, applying the plan.md tier table and the allow-list in FR-004. Tests: hand-written ASS samples that breach each rule.
- [ ] T028-04 `theme.rs` (the `pharos` theme: faces from the V530 pack, colours, outlines, the Plain style) and `compile.rs` (the script header, styles, and passthrough of `typeset` events unchanged, FR-005). Test: an all-Plain plan compiles to the Plain style only, and the same inputs give the same bytes (V532).
- [ ] T028-05 [P] `effects/impact.rs`, `tremor.rs`, `hush.rs`: low, tv and desktop forms. Golden ASS snippets per tier.
- [ ] T028-06 [P] `effects/inner_voice.rs`, `echo.rs`, `wire.rs`, `memory.rs`: the same.
- [ ] T028-07 [P] `effects/caption.rs`, `aside.rs`, `burst.rs`, `karaoke.rs` (Sweep, Bounce, Ember from source syllables). Sarcasm compiles as Plain until it has an effect. The same goldens.
- [ ] T028-08 Property test (V533, `PROPTEST_CASES` honoured): random plans over random source lines, for each tier and level. The output parses with `pharos_subs::decode` with `bad_lines == 0` (pharos-subs is a dev-dependency), `budget::check` passes, every event lies within its source line's time range, and Subtle caps intensity at 1.

## Slice 2 — signals op (B602; V534)

- [ ] T028-09 `pharos-transcode/src/subtitle_signals.rs`, pure DSP: K-weighting and R128 short-term loudness, YIN F0 (70-500 Hz, 40 ms frames), high-band share above 3.4 kHz, and the keyframe-cut proxy (`cuts_from_keyframes`, which drops the regular GOP cadence). Tests: synthetic tones and noise at known levels and pitches, and keyframe lists with and without cuts.
- [ ] T028-10 `TinyOp::SubtitleSignals` / `WorkerEvent::SubtitleSignalsResult` (appended last), `libav/subtitle_signals.rs` (one demux pass, front-centre channel when present, downmix otherwise), the `transcode_worker.rs` arm, `LibavWorkerPool::subtitle_signals`, and `is_heavy_op` with a timeout scaled to duration. Test in `it_libav`: a generated fixture with a loud window, a quiet window and two keyframe cuts. The file is opened once (V534).

## Slice 3 — director and job (B603; V535, V537)

- [ ] T028-11 `director.rs`: the ordered rules from plan.md, a `reason` per line, track medians, and the all-caps track guard. Table tests per rule, plus "no signal gives Plain".
- [x] T028-12 Migration `0068_subtitle_styling` (sqlite + postgres), `SubtitleStylingStore` in `pharos-core`, and the sqlx implementations. Run `just test-postgres`.
- [x] T028-13 `pharos-server/src/subtitle_styling.rs`: warm-up, the background-leader gate, waiting while `pharos_transcode_pending_jobs > 0`, `BgPermit` around signals, one item at a time, an on-demand queue ahead of the backfill, the genre gate (FR-015), source choice (FR-016), and version-only rows for undecodable files. Metrics `pharos_subtitle_styling_total{outcome,stage}`, `_seconds{stage}`, `_pending` and `pharos_subtitle_effect_total{intent,source}`; one info line per item and one debug line per line decision. Tests use a fake signals source: each outcome is counted once with its stage, and failures carry the same fields (V535).
- [ ] T028-14 A reference-episode report tool (`just styling-report <item>`) that prints 50 non-Plain lines per episode with their intents, for Alison to label (SC-002, V537: a manual gate).

## Slice 4 — serving (B604; V532)

- [ ] T028-15 `SubtitleKind::Styled { tier, level }` in `pharos-cache` (disk letter `y`, with the versions in the name).
- [ ] T028-16 Synthetic index in `services/watch.rs::tracks` for the native detail only, listed only while a current plan exists. The `stream.ass` branch takes `?tier=&effects=`, defaults to `tv` and sets `X-Pharos-Subtitle-Tier`. `stream.vtt`, `stream.js`, fonts and burn answer 404 for that index. Delivery is counted with `source="generated"` (extending B600's `DeliverySource`). Integration tests cover each route.

## Slice 5 — UI setting and starting tier (B605; V539)

- [ ] T028-17 [P] `EffectsLevel` pref (`pharos.subtitles.effects`, default Subtle) and the Appearance select (dioxus-ssr tests, as for V455).
- [ ] T028-18 [P] Device class and `starting_tier(device, reduced_motion, remembered)`, pure and table-tested. `web.rs` gets the UA and pointer detection plus `prefers-reduced-motion` through `platform::prefers`.
- [ ] T028-19 The player fetches the styled track with `?tier=&effects=`. Off hides the track. A change to reduced motion or the level reloads the script in place. e2e in Chromium and Firefox with muted/null audio.

## Slice 6 — runtime tier drop (B606; V536)

- [ ] T028-20 A pure tier-drop state machine (10 s window, 5% dropped-frame threshold, 30 s cooldown, Low floor) with unit tests.
- [ ] T028-21 `web.rs`: sample `getVideoPlaybackQuality` every 1 s while the styled track shows, persist the dropped tier per device, log it and send it as `rendererMode` (`renderer`, `from`, `to`, `reason`, the measured values). e2e with a fake quality source.
- [ ] T028-22 G08 run: dropped frames for the styled track against the plain track on a 5-minute fixture (SC-001), between 08:00 and 23:00 local.

## Ledger

- [ ] T028-23 Each slice's PR appends its B entry to `bugs.md` and its V entries to `invariants.md`, and marks ADR-0067 Accepted when slice 1 merges.
