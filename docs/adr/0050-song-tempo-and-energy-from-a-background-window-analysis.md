# ADR-0050: Song tempo and energy come from the BPM tag and a background analysis of a 30 s window

- **Status:** Accepted
- **Date:** 2026-11-04T10:30:00Z
- **Deciders:** Alison (asked for tempo and energy in mixes, "done efficiently as usual"), Claude

## Context

The dynamic mixes engine (spec 024, ADR-0048) wants "Tempo from–to BPM",
"Energy from–to" and tempo/energy likeness rules. The library held no tempo or
energy. Some files carry a BPM tag (ID3 `TBPM`, Vorbis `BPM`, MP4 `tmpo`); few
carry anything about energy. The deployment is a home server whose media sits
on NFS and whose CPUs also transcode video, so the cost that matters is how
many bytes are read and decoded, and whether that competes with playback.

## Decision

We will:

- Read the BPM tag and analyse the audio in **one worker op**,
  `TinyOp::AudioFeatures`, on the resident libav worker pool (V6). The tag is
  read before any decode; a usable tag skips the tempo estimator, and energy is
  always computed.
- Decode **only a 30 s window** from a third of the way in (whole song if
  shorter), downmixed to mono and resampled to 11 025 Hz as it is decoded.
- Estimate tempo with the onset-autocorrelation method (two-band log-energy
  onset envelope, smoothed, autocorrelation over 50–220 BPM, half-lag
  subdivision term, log-normal prior centred at 135 BPM, parabolic
  refinement). Below a confidence of 0.08 the tempo is left unknown.
- Compute energy as 0.5 · loudness (RMS dBFS mapped −32…−8) + 0.3 · brightness
  (first-difference share of RMS) + 0.2 · onset density (per second, /8),
  0.0 to 1.0.
- Write it all pure Rust with no new dependency (`pharos_transcode::audio_features`).
- Run it as a **background sweep** (`pharos_server::audio_analysis`): one song at
  a time, each decode behind the metered `bg_io` permit, background leader only.
- Store results in `media_items` columns that `put` never writes
  (`bpm_mille`, `tempo_source`, `energy_mille`, `audio_analysis_version`) with
  the file signature the analysis read (`audio_analysis_mtime`, `_size`). A
  read exposes them as `MediaProbe::audio_analysis` only while that signature
  equals the row's scan signature (V385). An analysis that found nothing, or a
  file the decoder refused, is stored as a version with no values.
- Expose `MediaProbe::bpm()`, `energy()` and `tempo_source()` as the seam the
  mixes engine reads. No native API (capnp) change.

## Consequences

- A library is analysed once. A re-probe (a `PROBE_SCHEMA_VERSION` bump)
  keeps results; a changed file reads as unanalysed with no write on the scan
  path, and is redone; `ANALYSIS_VERSION` redoes everything when the method
  changes.
- Reading tags in the sweep rather than the prober means no probe-version bump,
  which would have re-probed every video file to pick up a tag only songs have.
  The cost: a tagged BPM appears after the sweep reaches the song, not straight
  after the scan.
- One window can miss a tempo change or a quiet intro/loud outro; energy is a
  ranking signal, not a calibrated measurement.
- Like any such estimator, a steady beat outside about 72–174 BPM can read an
  octave off. Rules that compare tempos can treat half and double as close.
- The sweep holds at most one pool worker, so a large first pass is slow (hours
  for tens of thousands of songs) by design; it yields during playback.
- A file changed on disk since the last scan is skipped (`outcome="unscanned"`)
  until the scan stamps its new signature, so it is decoded once, after the
  scan, not once per pass.
- Damaged frames in the window are skipped as ffmpeg skips them; only a
  container libav cannot parse is stored as undecodable. An I/O error on open
  (an NFS blip) is retried, never stored.

## Alternatives considered

- **Probe-time tag read + per-kind version.** Needs a new music-only probe
  version and still needs the sweep for energy; two readers of one file.
- **A memo file beside the cache (the integrity sweep's shape).** Cheap to
  write, but mix rules need the values on the items they already load, and a
  tag is domain data, not a memo.
- **Analyse the whole song / several windows.** Several times the read and
  decode for a small gain in a ranking signal.
- **A crate (aubio bindings, bliss-audio, essentia).** C/C++ dependencies or a
  heavy feature set for two numbers; the method is about 200 lines and testable
  on synthetic beats.
- **Decode via the spawned `ffmpeg` binary.** A fork per song; the resident pool
  already exists for exactly this.

## References

- `crates/pharos-transcode/src/audio_features.rs`, `libav/audio_features.rs`
- `crates/pharos-server/src/audio_analysis.rs`
- `crates/pharos-store-sqlx/migrations/*/0064_audio_analysis.sql`
- spec 024 (`specs/024-music-mixes/spec.md`), ADR-0048
- Scheirer 1998, "Tempo and beat analysis of acoustic musical signals"; Ellis 2007, "Beat tracking by dynamic programming"
