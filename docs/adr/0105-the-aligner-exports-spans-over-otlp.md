# ADR-0105: The aligner exports spans over OTLP and joins the server's trace

- **Status:** Proposed
- **Date:** 2026-11-10T06:20:00Z
- **Deciders:** Alison Jenkins

## Context

pharos-aligner (ADR-0066) is the Python service that times karaoke words. On
2026-11-09 a 304 s song took 500 s to align, and the server had nothing to say
about where the time went: the aligner wrote one log line per song, and only
when the song finished. A slow answer could have been queueing behind another
song, ffmpeg decoding, the wav2vec2 forward pass, or the CTC search. When the
server's 600 s timeout cut the request off, the aligner said nothing at all.
The server already exports its spans to Tempo over OTLP/gRPC
(`[obs].otlp_endpoint`). Alison asked for tracing in the aligner to support
observability-driven development.

## Decision

The aligner uses the OpenTelemetry Python SDK (`opentelemetry-api`,
`opentelemetry-sdk`, `opentelemetry-exporter-otlp-proto-grpc`, from nixpkgs).

- With `PHAROS_ALIGNER_OTLP_ENDPOINT` set, it exports spans over OTLP/gRPC as
  service `pharos-aligner`. The chart sets this from `config.obs.otlpEndpoint`,
  the same endpoint the server uses.
- Unset, the API's no-op tracer makes every span free.
- The server's alignment job runs in a `lyrics.align` span and sends a W3C
  `traceparent`, so `aligner.align` is that span's child.
- Spans:
  - `aligner.wait`: queued behind another song.
  - `aligner.decode` and `aligner.model`: tagged with `stage` (probe or line).
  - `aligner.forward` and `aligner.ctc`: inside the model span.
- The per-phase totals go on the request span and on the song's log line
  (`decode_ms=… model_ms=…`).
- A request whose caller has gone logs `failed=caller_gone` with the same
  totals.

## Consequences

- A slow song can be read off one trace in Tempo, next to the server's request
  that started it.
- The image gains the SDK, the gRPC exporter and grpcio: a few MB next to
  torch, and a few MiB of RSS. Spans are batched off the request thread.
- Tests depend on the SDK's in-memory exporter.

## Alternatives considered

- **Phase timings in logs only:** no extra dependency, but no link to the
  server's trace and no per-line breakdown, so it answers less than a trace.
- **OTLP/HTTP exporter:** lighter, but the cluster's endpoint is gRPC (4317),
  and a second endpoint setting would be easy to get wrong.

## References

- ADR-0066, B1164, V1124
- `aligner/pharos_aligner/tracing.py`
- `crates/pharos-server/src/obs.rs` `traceparent`
