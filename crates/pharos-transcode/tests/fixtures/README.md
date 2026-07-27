# Fingerprint fixtures

## `mushoku_s03_fingerprints.txt`

Real audio fingerprints dumped from the production store on **2026-07-27**, kept so
the intro-detection recall failure is reproducible in CI without media, NFS or a
database.

**Source**: `episode_fingerprints` for the five episodes of
`/var/lib/pharos/media/TV/Mushoku Tensei - Jobless Reincarnation/Season 03`:

| `item_id` | Episode |
|-----------|---------|
| 1283239092009329305 | S03E04 |
| 3096759618643281933 | S03E02 |
| 5164188882487917131 | S03E03 |
| 6463723843998048628 | S03E01 |
| 7093100027006938661 | S03E05 |

**Format**: one row per `(item_id, kind)`, space separated:

```
<item_id> <kind> <hex>
```

- `kind` is `intro` or `credits` (lowercase — the *fingerprint* vocabulary;
  `media_segments.kind` uses `Intro`/`Outro` and is a different vocabulary).
- `hex` is `episode_fingerprints.points` verbatim: little-endian `u32` per
  fingerprint point, each point covering ≈0.2476 s of audio. Decode an 8-hex-char
  group with `u32::from_str_radix(.., 16).swap_bytes()`.

Intro windows are 1412–1413 points (≈350 s); credits windows are 1794–1796 points
(≈445 s).

**Why this season**: it is the one a viewer reported. Detection emitted a closing
for 3 of 4 episodes and `no_span` for every episode's opening, on the same files in
the same pass — so it isolates an intro-side defect with a built-in control.

**Regenerating** (read-only, needs cluster access):

```bash
kubectl -n pharos exec pharos-db-1 -c postgres -- \
  psql -U postgres -d pharos -tAc \
  "select item_id||' '||kind||' '||encode(points,'hex')
     from episode_fingerprints
    where item_id in (1283239092009329305, 3096759618643281933,
                      5164188882487917131, 6463723843998048628,
                      7093100027006938661)
    order by item_id, kind"
```

Note: episode `3096759618643281933` (S03E02) matches nothing in either kind, in
production and in replay. That is expected for this fixture, not a decoding fault.
