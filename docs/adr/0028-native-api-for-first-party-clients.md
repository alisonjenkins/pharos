# ADR-0028: pharos-ui speaks a pharos-native API; the Jellyfin API remains for third-party clients

- **Status:** Accepted
- **Date:** 2026-11-01T11:05:00Z
- **Deciders:** Alison

## Context

ADR-0001 made the Jellyfin API the client contract, and V16 then bound the
first-party Dioxus UI to it: "consumes only public Jellyfin-compat API … UI
swappable for any Jellyfin client." That served Phase 1, getting stock
Jellyfin clients working, and it remains the reason phones and TVs "just work".

pharos was always meant to be the next generation, not a Jellyfin clone
(Alison, 2026-11-01). The modern UI (specs 009–016) needs things the Jellyfin
API has no shape for, or does inefficiently:

- many REST round trips per screen, and JSON payloads the wasm UI must parse;
- a separate `/socket` multiplex for push;
- no typed home for pharos-only features: opt-in telemetry with trace
  correlation (016), themes and keymaps (010, 011), group chat (014), remote
  sources (008), cache/transcode admin (015);
- Jellyfin-shaped data (ticks, PascalCase, string ids) where pharos has
  better types (constitution V).

pharos will also ship its own phone and TV apps later; the web UI is the first
client of whatever replaces the Jellyfin API for first-party use.

## Decision

1. **pharos-ui uses a pharos-native API**, schema-first on Cap'n Proto
   (spec 017), not the Jellyfin REST API. Future pharos phone and TV apps use
   the same API.
2. **The Jellyfin API remains a fully supported compatibility surface** for
   third-party clients. Principle I, V1 and V7 are unchanged, and no Jellyfin
   endpoint changes because of the native API.
3. **Both surfaces are thin wire adapters over shared domain services.** Logic
   currently living in `api/jellyfin/*` handlers moves into services as each
   native method needs it, with the existing Jellyfin tests as the regression
   net. Neither wire layer holds business logic the other lacks.
4. **Bulk media stays HTTP** (HLS, streams, images, trickplay, subtitles, book
   files). The native API hands out their URLs and capability tokens.
5. The native API is **public and documented**, not a backdoor: any client may
   use it, and its schema is the documentation.

6. The native API is **observable at parity with HTTP**, by construction:
   instrumentation is generated into the dispatch layer, and decoded,
   redacted views of calls exist for logs, a CLI and the UI debug overlay, so
   the binary wire is never opaque (V173, spec 017 §Observability).

V16 is amended in place to state this, V172 records the shared-services rule
and V173 the observability rule.

## Consequences

- One efficient, typed, push-capable contract for every first-party client, with
  pipelining (fewer round trips) and capability-based access.
- pharos-ui is no longer swappable for an arbitrary Jellyfin client. jellyfin-web
  stays deployed beside it at `/web/` for anyone who wants a Jellyfin client.
- Two wire surfaces to keep correct. That is mitigated by shared services and by
  running the full Jellyfin compat suite on every change.
- New toolchain pieces: `capnp` compiler in the devShell, a schema crate built
  for native and wasm, a schema-compat CI check.
- The migration is incremental: pharos-ui's `PharosApi` port (009) keeps its
  current Jellyfin REST client as a transitional adapter until every behaviour
  has a native method.

## Alternatives considered

- **Stay on the Jellyfin API (V16 as written).** Rejected: it caps the UI at
  Jellyfin's shapes and efficiency and has no home for pharos-only features.
- **Jellyfin API plus `/Pharos/*` REST extensions with Jellyfin fallbacks**, the
  first draft of this ADR in the same PR, never merged. Rejected: it still treats
  pharos-ui as a Jellyfin client, the opposite of the goal.
- **Protobuf/gRPC-web, FlatBuffers, JSON over WebSocket.** To be compared in
  017's `research.md`. Cap'n Proto is the stated preference for its pipelining,
  capabilities and zero-copy reads.

## References

- `specs/017-native-api/spec.md`; `docs/ui-gap-assessment.md` §6
- ADR-0001 (Jellyfin API contract, still governs third-party clients),
  ADR-0011 (token store, shared), ADR-0012 (Dioxus UI), ADR-0014/0016 (group sync)
- `.specify/memory/constitution.md` v1.1.0; invariants V16, V172, V173
