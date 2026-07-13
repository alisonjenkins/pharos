# ADR-0017: Adaptive background-I/O gate (shared semaphore, parks while streaming)

- **Status:** Accepted
- **Date:** 2026-07-13T00:00:00Z (gate live 2026-07-12; backfill v2 2026-07-13)
- **Deciders:** Alison

## Context

ADR-0007 protected live playback from background whole-file decodes with a
**binary quiet gate** (`await_gate`): background work parked until streaming
had been idle 30s. Two problems emerged in production:

1. **All-or-nothing starves the backlog.** Any evening of continuous viewing
   halted trickplay/subtitle backfill completely; a 12k-item library would
   never converge on a server that is regularly watched.
2. **It gated the wrong resource.** The real contention is *concurrent NFS
   reads + decode CPU*, which is a capacity question, not a boolean. One
   background decode alongside a stream is harmless; eight are not. Separately,
   an unrelated incident (library scans starving playback I/O) needed the same
   throttle — the gate concept was bigger than trickplay.

## Decision

Replace the binary gate with a single **shared adaptive semaphore**
(`AppState::bg_io`) that every background whole-file I/O consumer acquires:
scan probes, subtitle warm-demuxes, trickplay pre-generation.

- Capacity is **8 permits when idle**. A regulator task watches playback
  activity (12s busy window) and **parks all but 1 permit while anyone is
  streaming**, restoring full capacity when playback stops. Background work
  degrades to a trickle instead of halting — the backlog always advances.
- The **priority-tier seed bypass survives** from ADR-0007: the item being
  watched right now skips the gate entirely, since its previews are wanted by
  the very session holding the gate down.
- The trickplay backfill itself was restructured (2026-07-13) into two tasks:
  a **priority worker** (nudged by PlaybackInfo + progress reports; expands to
  the watched series in watch order) and a **sweep** (newest-first over the
  whole library, concurrency 10, deliberately queue-less — the tiles on disk
  are the durable progress record, so a restart loses nothing). DTOs advertise
  only widths whose tiles actually exist, so clients never render an empty
  scrub preview (SPEC B35).

## Consequences

- Playback keeps near-exclusive I/O when it matters, while the library
  backfill converges even on a frequently-watched server (~65 tiles/hour
  measured with viewers active; full 12k coverage in days, not never).
- One knob governs every background consumer — a new background whole-file
  job must acquire `bg_io` or it reintroduces the starvation class.
- The single busy-window heuristic is coarse: one lightweight audio stream
  parks the gate as aggressively as three video transcodes. Acceptable at
  current scale; refine only with evidence.
- Multi-replica (ADR-0015) runs one sweep per replica — duplicate NFS decode
  work, tracked as SPEC §T T85 (gate the sweep behind bg-leader election).

## References

- ADR-0007 (pre-generation model — its gate mechanism is superseded by this)
- `crates/pharos-server/src/state.rs` (`bg_io`, `spawn_bg_io_regulator`),
  `trickplay_backfill.rs`
- `SPEC.md` §B (B34, B35), §T (T82, T85); memory `project_pharos_adaptive_io_gate`
