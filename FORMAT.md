# FORMAT.md — SPEC.md encoding

Caveman pipe-tables. Drop articles/filler. Fragments OK. Identifiers/paths/code verbatim.

## Sections

- **§G** goal — 1-3 lines. What thing do, why exist.
- **§C** constraints — bullets. Stack, compat, perf, scope limits.
- **§I** interfaces — bullets `name — role`. External surfaces: APIs, files, configs, CLIs.
- **§V** invariants — numbered `V<N>: <rule>`. Must hold post-build. 1 line each.
- **§T** tasks — pipe table `id|status|desc|cites`.
- **§B** bugs — pipe table `id|date|cause|fix`.

## Status glyphs (§T)

- `.` todo
- `~` in progress
- `x` done
- `!` blocked

## Cites column (§T)

Comma-list `V<N>` and `I.<name>` deps. Example: `V2,V7,I.jellyfin-api`.

## Numbering

Monotonic. Never reuse V/T/B ids. Append only.

## Caveman

Drop: a/an/the, just/really/basically, please/thanks. Use: fragments, short synonyms (big/fix/use). Keep: code, paths, identifiers, error strings exact. Keep: technical terms exact.
