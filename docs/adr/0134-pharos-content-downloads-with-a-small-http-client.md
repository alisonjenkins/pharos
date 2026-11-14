# ADR-0134: pharos-content downloads audio with its own small HTTP/1.1 client

- **Status:** Accepted
- **Date:** 2026-11-14T17:39:59Z
- **Deciders:** Alison

Backfilled: the client was written with the crate, without an ADR. Reasons
nobody wrote down are marked "not recorded".

## Context

The phone and car app caches music for offline play and reads songs for the
visualiser (ADR-0133). Both need a streamed GET with an optional `Range` and
`If-Range`, from a Jellyfin-shaped server, over TLS. Stream URLs carry the
media token as `api_key`, so no error or log line may print a URL.
`pharos-content` already links rustls (ring provider, bundled webpki roots)
for its websocket link.

## Decision

We will keep a hand-written HTTP/1.1 GET client in
`crates/pharos-content/src/http.rs`. It sends one request per connection,
frames the body as `Content-Length`, chunked or close-delimited, parses the
head with `httparse`, and uses `tokio-rustls` with the same roots as the
websocket. Cleartext is accepted for loopback only. Errors carry the host and
the failing step, never the URL. A `Content-Length` that does not parse fails
the response.

## Consequences

- Easier: the error type names the host and step by construction, redaction
  of `api_key` is in one place, and the TLS stack is the one already linked.
- Harder: we own framing, chunked decoding and truncation detection. A
  close-delimited body cannot be told from a complete one, so the downloader
  relies on the advertised length where there is one. No redirects, no
  proxies, no HTTP/2, no connection reuse.
- Follow-up: any new server behaviour (redirects, compression) is ours to add
  or a reason to revisit this ADR.

## Alternatives considered

- A general HTTP client crate: why it lost is not recorded.

## References

- `crates/pharos-content/src/http.rs`
- ADR-0133 (the visualiser reads songs through this client)
