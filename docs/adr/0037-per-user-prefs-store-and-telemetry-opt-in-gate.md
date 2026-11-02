# ADR-0037: Per-user prefs live in their own store, and Telemetry.submit enforces the diagnostics opt-in

- **Status:** Accepted
- **Date:** 2026-11-02T15:30:00Z
- **Deciders:** Alison Jenkins

## Context

Spec 016 US9 makes UI telemetry off by default for every user; users opt in.
The first implementation kept the choice in the browser's localStorage, which
the server cannot see, so `Telemetry.submit` accepted batches from any signed-in
client. The native API already declares `Prefs.get` / `Prefs.set` (017 T068),
and the 010 themes and 011 keymaps also need a per-user place to live.

`PreferenceStore` holds Jellyfin's `UserConfiguration` and display preferences:
JSON documents keyed by display-preference id and client, shaped by
jellyfin-web. A flat per-user key with an opaque value does not fit that shape.

## Decision

- A new narrow trait, `pharos_core::UserPrefStore` (`get_user_pref`,
  `set_user_pref`), over a `user_prefs (user_id, key, value, updated_at)` table
  (migration 0060 on sqlite and postgres, cascading with the user). Values are
  opaque bytes; the store holds no policy.
- Policy sits in `rpc/prefs.rs`: an allow-list of keys (today only
  `diagnostics.opt_in`) and a 4096-byte value cap. A refused call is a
  `notPermitted` or `limit` error and a WARN with the key and size, never the
  value.
- `Telemetry.submit` reads the session user's `diagnostics.opt_in` (the bytes
  `true`; absent or anything else is off). Not opted in: the batch is dropped,
  answered `notPermitted`, counted in
  `pharos_client_events_dropped_total{reason="not_opted_in"}`. The session caches
  the pref for 5 s; a `Prefs.set` of the key on the same session drops the
  cache, so an opt-out applies to the next batch there and within 5 s from
  another device.
- The UI keeps the choice in `Prefs`, and keeps its own client-side gate from an
  in-memory copy.

## Consequences

- Opt-in is enforced by the server, not only by a polite client.
- Later keys (theme, keymap) add a name to the allow-list; no migration.
- A pref write from another device can lag the gate by up to 5 s.
- The sqlite-to-postgres migration tool (`migrate.rs`) does not copy
  `user_prefs` yet; a migrated install starts with every pref unset, which for
  diagnostics means off.
- An old localStorage opt-in is removed, not carried over: consent given to a
  browser is not consent recorded on the server.

## Alternatives considered

- Extending `PreferenceStore`: its methods are Jellyfin-specific and every
  implementor would grow methods unrelated to it.
- A JSON-document-per-user pref column: whole-document writes race between
  devices, and a size cap per key is harder to state.
- Gating only in the client: unenforceable against an old or modified client.

## References

- `specs/016-ui-observability/spec.md` US9; `specs/017-native-api/tasks.md` T068, T069
- `crates/pharos-core/src/lib.rs` (`UserPrefStore`), `crates/pharos-server/src/api/pharos/rpc/prefs.rs`, `rpc/telemetry.rs`
