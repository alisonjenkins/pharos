# ADR-0018: Automatic intro / outro detection (audio fingerprinting)

- **Status:** Proposed
- **Date:** 2026-07-14T00:00:00Z
- **Deciders:** Alison
- **Task:** §T T86

## Context

pharos already serves `GET /MediaSegments/{itemId}` (T64), driving jellyfin-web
≥10.10's native **Skip Intro / Skip Outro** overlay. But T64 only derives
segments from **labeled chapter markers** (`classify_chapter_title`) — a
chapter titled "Opening"/"Credits"/etc. Real-world TV rips (Code Geass,
Supergirl) carry no such chapters, so `/MediaSegments` returns empty and no
skip button ever appears. Verified live 2026-07-14: Supergirl S01E01 + S01E03
both return 0 segments.

The gold-standard solution is the Jellyfin **intro-skipper** plugin
(`intro-skipper/intro-skipper`, formerly ConfusedPolarBear): it audio-
fingerprints every episode of a season and finds the span of audio repeated
across episodes — that repeated span *is* the intro (and, at the tail, the
credits/outro). This ADR documents that algorithm as-built, then specifies the
pharos port and where we improve on it.

## The intro-skipper algorithm (as researched, plugin branch 10.11)

### 1. Fingerprint generation

Per episode, chromaprint fingerprints are computed over a time window:

```
ffmpeg -ss <start> -i <path> -to <duration> -ac 2 -f chromaprint -fp_format raw -
```

- Output is a raw byte stream; every **4 bytes = one `u32` fingerprint point**
  (little-endian).
- Each point covers **`SampleDuration = 4096/11025/3 ≈ 0.12383 s`** of audio
  (chromaprint: 11025 Hz mono-ish chroma, 4096-sample frame, ⅓ hop) → ~8.08
  points/second.
- **Intro window:** the first `AnalysisPercent = 25 %` of the episode, capped
  at `AnalysisLengthLimit = 10 min`.
- **Credits window:** the tail, from `CreditsFingerprintStart` (episode
  duration − `MaximumCreditsDuration`; TV = 450 s, movie = 900 s) to the end.

### 2. Pairwise comparison (`ChromaprintAnalyzer.CompareEpisodes`)

Two episodes' fingerprint arrays are aligned and matched:

**a. Candidate shift discovery via an inverted index.** Build
`Dictionary<u32 point → last index>` for each side. For every lhs point, probe
`rhs` for `point ± InvertedIndexShift` (=2) — fuzzy value match — and record
the candidate offset `rhsIndex − lhsIndex`. This yields the handful of
alignment offsets worth testing (vs brute-forcing every shift).

**b. Point matching.** At a candidate shift, walk both arrays in lockstep;
two points match when `popcount(lhs[i] ^ rhs[j]) ≤ MaximumFingerprintPoint`
`Differences` (=**6** bits out of 32). Matching point *times* accumulate into
a list.

**c. Contiguous-region expansion.** `FindContiguous(times, MaximumTimeSkip)`
(=**3.5 s** max gap) collapses the matched times into the longest run where
consecutive matches are ≤3.5 s apart. That run's `[start,end]` is the candidate
intro.

**d. Bounds + snap.** Reject if `Duration < MinimumIntroDuration` (=15 s) or
`> MaximumIntroDuration` (=120 s). If the region starts ≤ **5 s**, snap start
to 0. Credits: add `CreditsFingerprintStart` back to both endpoints (the tail
fingerprint was zero-based).

### 3. Season aggregation (`AnalyzeMediaFiles`)

Episodes are grouped by season. Each episode is compared **pairwise** against
every other episode in the season; the **longest** matching span found for a
given episode is kept. A single-episode season falls back to comparing against
adjacent-numbered episodes. Results are time-adjusted (`TimeAdjustmentHelper`)
and persisted.

### 4. Black-frame credits (`BlackFrameAnalyzer`, secondary)

For credits specifically, a black-frame detector refines/replaces the audio
result: binary-search the tail for the boundary where frames become black
(`BlackFrameThreshold` = 28 luma, `BlackFrameMinimumPercentage` = 85 % of
pixels), which marks the credits roll start. Robust for shows whose credits
aren't musically identical across episodes.

### 5. Config defaults (the full knob set)

| Constant | Default | Meaning |
|---|---|---|
| `MaximumFingerprintPointDifferences` | 6 | bit-diff match threshold (of 32) |
| `InvertedIndexShift` | 2 | fuzzy point-value probe range ± |
| `MaximumTimeSkip` | 3.5 s | max gap inside a contiguous match |
| `MinimumIntroDuration` | 15 s | reject shorter intros |
| `MaximumIntroDuration` | 120 s | reject longer intros |
| `MinimumCreditsDuration` | 15 s | reject shorter credits |
| `MaximumCreditsDuration` | 450 s (TV) / 900 s (movie) | tail window fingerprinted |
| `AnalysisPercent` | 25 % | head fraction fingerprinted for intro |
| `AnalysisLengthLimit` | 10 min | cap on head window |
| `BlackFrameThreshold` | 28 | luma ≤ this = black pixel |
| `BlackFrameMinimumPercentage` | 85 % | black-pixel fraction = black frame |

## Decision

Port the algorithm into pharos as a new analyzer that generates typed
`MediaSegment`s (Intro + Outro) per episode, persisted in the store, generated
by the existing bg-IO-gated backfill, and served through the *existing*
`/MediaSegments` endpoint (T64) — chapters remain the fast path, fingerprinting
fills the gap.

### Fingerprinting — pure-Rust, in-process (improvement #1)

The plugin forks `ffmpeg -f chromaprint` per episode and depends on
jellyfin-ffmpeg (our `ffmpeg-headless` has **no** chromaprint muxer; `fpcalc`
is absent too). Instead:

- Decode the head/tail audio window to mono PCM **in the persistent libav
  worker pool** (the same crash-isolated pool that already does
  `waveform_rms` audio decode — V6), and feed PCM to the **`rusty-chromaprint`**
  crate (pure-Rust, AcoustID-compatible; `chromaprint-next` is a bit-identical
  alternative). A new `TinyOp::Fingerprint { input, start_ms, dur_ms }` returns
  the `Vec<u32>`.
- No ffmpeg-build dependency, no fork/exec per episode, fault-isolated, and it
  reuses the pool + bg-IO gate we already run.

### Alignment + season flow

Reimplement `CompareEpisodes` (inverted-index shift discovery →
popcount-≤6 matching → `FindContiguous` with 3.5 s skip → 15–120 s bounds →
≤5 s snap) verbatim in a `pharos-analysis` module, with the plugin's default
constants (validated, tune later). Group by our synthetic season key
(`series_id_for_key` folder identity, `season_number`), pairwise-compare,
keep the longest per episode, for **both** the head window (Intro) and the tail
window (Outro/Credits).

### Storage

New store table `media_segments(item_id, seg_type, start_ticks, end_ticks,
detector, schema_version)` + a `episode_fingerprints(item_id, kind, points
BLOB, schema_version)` cache so re-detection when a new episode lands doesn't
re-fingerprint the whole season. Both `PROBE_SCHEMA_VERSION`-adjacent
(bump a dedicated `SEGMENT_SCHEMA_VERSION` to force re-analysis on algorithm
change). sqlite + postgres migrations.

### Generation (backfill)

A season-level analyzer rides the existing bg-IO-gated backfill next to
trickplay (ADR-0017): when a season has ≥2 unanalyzed episodes and the box is
idle, fingerprint the missing episodes (gated), run the pairwise pass, persist
segments. Gated so it never starves live playback — the exact failure that
plagued B49/B52. Behind the bg-leader election for multi-replica (like the
trickplay sweep, T85).

### Serving

`build_media_segments` (T64) gains a second source: after the chapter pass,
UNION persisted fingerprint/black-frame segments (chapters win on conflict —
they're exact). Wire shape unchanged.

## Improvements over the plugin

1. **Pure-Rust in-process fingerprinting** on the libav pool — no
   jellyfin-ffmpeg chromaprint dependency, no per-episode fork, crash-isolated,
   already-gated. (Above.)
2. **Reference-fingerprint fast path.** Once a season's intro is agreed, store
   its fingerprint slice as the season's *reference*. A newly-added episode
   matches against the reference in **O(1)** rather than re-running the O(n²)
   pairwise pass — near-instant incremental detection as episodes arrive.
3. **Series-wide reuse.** Intros are usually identical across seasons of a
   show. Fall back to the *series* reference when a season has too few episodes
   (the plugin's adjacent-episode fallback is weaker), fixing the 1–2 episode
   season case.
4. **Consensus + confidence.** Instead of "longest match per episode", cluster
   all pairwise spans and emit the consensus span with a confidence score;
   drop outliers (a coincidental musical match in one pair no longer sets a
   bogus intro). Confidence is persisted, surfaced in logs, and gates whether
   the segment is served.
5. **Adaptive gating** beats the plugin's scheduled-task model: generation
   yields to live playback via the shared bg-IO semaphore, and picks up where
   it left off (fingerprints are cached), so a watched server still converges.
6. **Chapters + fingerprint + black-frame, layered.** T64 chapters (exact,
   free) → audio fingerprint (intro + outro) → black-frame refine (credits
   roll). Best available source wins per segment; the plugin treats these as
   competing analyzers, we compose them.

## Consequences

- New crate/module `pharos-analysis` (fingerprint + alignment; pure, unit-
  testable with synthetic fingerprint vectors — no ffmpeg in the hot tests).
- New deps: `rusty-chromaprint` (or `chromaprint-next`). Audit via cargo-deny;
  refresh `workspace-hack`.
- Two new store tables + migrations (sqlite + postgres) + a schema-version
  constant.
- One new `TinyOp` variant across the worker protocol (like B46's
  `SubtitleWindows`).
- Backfill CPU cost: fingerprinting decodes ~25 % of each episode's audio once
  ever (cached), gated — comparable to one subtitle warm per episode.
- Detection is heuristic: wrong/'missing segments possible on shows with
  variable intros or musical-score-heavy openings; confidence gating + the
  chapter fast path bound the blast radius, and a segment is never worse than
  "no button".

## Alternatives considered

- **fpcalc / ffmpeg-chromaprint muxer** (what the plugin uses): rejected — adds
  a chromaprint-enabled ffmpeg build + per-episode fork; the pure-Rust path is
  cleaner and reuses our pool.
- **Black-frame-only credits** (cheap first cut, no fingerprinting): kept as a
  *refinement* layer, not the primary — it can't detect intros and misses
  credits without clean black bookends.
- **Silence detection**: rejected as primary — too many false boundaries in
  dialogue.

## References

- intro-skipper plugin: <https://github.com/intro-skipper/intro-skipper>
  (`IntroSkipper/Analyzers/ChromaprintAnalyzer.cs`, `BlackFrameAnalyzer.cs`,
  `FFmpeg/FFmpegService.cs`, `Data/ChromaprintConstants.cs`,
  `Configuration/PluginConfiguration.cs`, branch `10.11`).
- rusty-chromaprint: <https://github.com/darksv/rusty-chromaprint> ·
  chromaprint-next: <https://github.com/attilagyorffy/chromaprint-next>
- Chromaprint algorithm: <https://acoustid.org/chromaprint>
- Related: ADR-0017 (bg-IO gate), §T T64 (MediaSegments/chapters), T86.
