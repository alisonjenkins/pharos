"""OpenTelemetry tracing for the aligner (ADR-0105).

With PHAROS_ALIGNER_OTLP_ENDPOINT set, spans go to it over OTLP/gRPC, and a
request carrying the server's `traceparent` joins the server's trace. Unset,
the OpenTelemetry API's no-op tracer makes every span free.
"""

from __future__ import annotations

import time
from contextlib import contextmanager
from typing import Any, Iterator, Mapping

from opentelemetry import propagate, trace
from opentelemetry.context import Context

# A proxy until `init` installs a provider, so spans started by modules that
# imported it before `init` ran still reach the exporter.
tracer = trace.get_tracer("pharos_aligner")


def init(endpoint: str | None) -> bool:
    """Export spans to `endpoint` over OTLP/gRPC; False (and no export) if unset."""
    if not endpoint:
        return False
    from opentelemetry.exporter.otlp.proto.grpc.trace_exporter import OTLPSpanExporter
    from opentelemetry.sdk.resources import Resource
    from opentelemetry.sdk.trace import TracerProvider
    from opentelemetry.sdk.trace.export import BatchSpanProcessor

    provider = TracerProvider(resource=Resource.create({"service.name": "pharos-aligner"}))
    exporter = OTLPSpanExporter(endpoint=endpoint, insecure=not endpoint.startswith("https://"))
    provider.add_span_processor(BatchSpanProcessor(exporter))
    trace.set_tracer_provider(provider)
    return True


def remote_parent(headers: Mapping[str, str]) -> Context:
    """The caller's span named by a W3C `traceparent` header, else a fresh context."""
    value = headers.get("traceparent")
    return propagate.extract({"traceparent": value} if value else {})


class Phases:
    """Wall time spent in each phase of one alignment, summed over its lines.

    Each `phase` is also a span, so a trace shows where the time went line by
    line while the totals go on the request's span and its log line.
    """

    def __init__(self) -> None:
        self.ms: dict[str, int] = {}

    @contextmanager
    def phase(self, name: str, **attributes: Any) -> Iterator[trace.Span]:
        started = time.monotonic()
        with tracer.start_as_current_span(f"aligner.{name}", attributes=attributes) as span:
            try:
                yield span
            finally:
                elapsed = int((time.monotonic() - started) * 1000)
                self.ms[name] = self.ms.get(name, 0) + elapsed

    def summary(self) -> str:
        """`decode_ms=… model_ms=…` in a stable order, for the log line."""
        return " ".join(f"{name}_ms={self.ms[name]}" for name in sorted(self.ms))
