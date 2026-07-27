# Quickstart: reproduce, fix, verify

**Plan**: [plan.md](./plan.md) · **Research**: [research.md](./research.md)

Everything here runs inside the Nix devShell (`nix develop --command …`).

## 1. Reproduce the failure without media or a cluster

The detector's real inputs are checked in:
[fixtures-mushoku-s03.txt](./fixtures-mushoku-s03.txt) — 10 rows,
`item_id kind hex`, hex being little-endian `u32` fingerprint points as stored in
`episode_fingerprints.points`.

Expected replay result on today's code (`align::compare` over all 10 pairs of each
kind, `AlignConfig::default()`):

| Kind | Matched pairs | Note |
|------|---------------|------|
| `intro` | **1 of 10** — only pair 0×4, 60.2 s at 136.2 s | 9 pairs report **0 candidate shifts** |
| `credits` | 6 of 10 — 90.9 s at ≈353.8 s | the real ending; agrees with the persisted rows |

The failing pairs die at candidate-shift discovery: no shift, so no run, so no
bounds check. `too_long` and `too_short` are both zero — nothing was rejected,
because nothing was ever built.

**This is the red test.** Land it before the fix and watch it fail; a fix is only
believable if this fixture goes from 1 matched intro pair to the same order as
credits.

## 2. Regenerate the fixture (only if it must be refreshed)

Needs cluster access. Read-only.

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

Decode each 8-hex-char group with `u32::from_str_radix(.., 16).swap_bytes()` to
undo the little-endian storage.

## 3. Run the tests

```bash
nix develop --command just test-fast                       # tight loop
nix develop --command cargo nextest run -p pharos-transcode  # detection only
nix develop --command just test                            # before any commit
```

`just test-postgres` is **not** needed unless an `sqlx::query*` string changes —
this feature is not expected to touch one.

## 4. Check for a cost regression

Alignment is `O(n²)` pairwise per season and the fix widens the per-pair shift
search. Budget it before merging:

- ≈1400 points per intro window → ≈2800 shifts × ≤1400 comparisons ≈ 4M
  XOR+popcount per pair.
- A 20-episode season is 190 pairs per kind.

Bench or time a full-season replay from the fixture pattern; a season must stay in
the low seconds. If it does not, R3's bucketed-seed fallback is the next option.

## 5. Verify in production, by query

After deploying, do not assert — run the queries in
[contracts/detection-signals.md](./contracts/detection-signals.md) and report the
actual output.

1. Confirm the deployed image is the one with the fix.
2. Wait for a sweep pass (30 min interval, 90 s warmup after boot).
3. Recall by kind — the number this feature moves:
   ```promql
   sum by (kind) (pharos_segment_detect_total{outcome="emitted"})
     / ignoring(outcome) sum by (kind) (pharos_segment_detect_total)
   ```
4. Ground truth — closing-only seasons, **145 before the fix**:
   ```sql
   -- full query in contracts/detection-signals.md
   ```
5. Named season, end to end:
   ```logql
   {namespace="pharos",container="pharos"} |= "detection verdicts" |= "Mushoku"
   ```
   Expect `kind=Intro` to move off `no_span=4`.
6. The user-visible check that closes the loop: play Mushoku Tensei S03 on the
   Google TV app and confirm **Skip Intro** appears, over the same window the
   browser shows.

## Gotchas

- `episode_fingerprints.kind` is lowercase (`intro`/`credits`);
  `media_segments.kind` is capitalised (`Intro`/`Outro`). Different vocabularies —
  do not compare them directly.
- Do not bump the fingerprint schema version. The cached fingerprints are correct;
  only the comparison changes. Invalidating them forces a full-library NFS
  re-read for no benefit (R5).
- Episode `3096759618643281933` (S03E02) matches nothing in either kind, in
  production and in the replay. Expect it to stay unmatched; confirm it is not one
  of the known zeroed library files before treating that as normal.
- The pod is distroless — use `kubectl port-forward`, not `exec`, for HTTP checks.
