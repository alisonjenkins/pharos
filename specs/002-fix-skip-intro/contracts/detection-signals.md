# Contract: detection observability

This is the interface the fix is **verified through** (constitution III: verify by
query, not by assertion). It exists today — V75 — and is the only reason this bug
was diagnosable. It must survive the fix intact.

## Metric

```
pharos_segment_detect_total{kind, outcome}
```

| Label | Domain | Source |
|-------|--------|--------|
| `kind` | `Intro` \| `Outro` | the window being analysed |
| `outcome` | `emitted` \| `no_span` \| `few_agreeing` \| `low_confidence` | `Verdict::label()` |

- Incremented once per **episode per kind** per analysis pass.
- The label set is a dashboard contract: bounded, stable, and sourced only from
  `Verdict::label()`. Adding a variant is a dashboard change and needs its own
  justification — it is not a free side effect of a fix.

## Log line

Emitted once per season per kind, at INFO (the level production runs at):

```
segment backfill: season detection verdicts
  season=<library path>::<season>
  kind=Intro|Outro
  episodes=<n> emitted=<n> low_confidence=<n> few_agreeing=<n> no_span=<n>
  dropped="<id>:<outcome>(<matched> matched/<agreeing> agreeing/conf <x.xx>) …"
```

The `dropped` field carries the **offending values**, not a bare class: an episode
nine peers agreed with is a different miss from one nothing matched, and the
aggregate cannot tell them apart. That distinction is what identified this bug —
`0 matched / 0 agreeing / conf 0.00` on every episode ruled out the consensus
layer immediately.

## Queries

Prove the bug is happening (this is the query that found it):

```logql
{namespace="pharos",container="pharos"} |= "detection verdicts" |= "<show name>"
```

Library-wide ratio, before and after:

```promql
sum by (kind, outcome) (pharos_segment_detect_total)
```

Recall by kind — the single number this feature moves:

```promql
sum by (kind) (pharos_segment_detect_total{outcome="emitted"})
  / ignoring(outcome) sum by (kind) (pharos_segment_detect_total)
```

Ground truth in the store, independent of the metrics pipeline:

```sql
-- seasons with a closing and no opening: 145 before the fix
select count(*) from (
  select regexp_replace(i.path,'^.*/TV/([^/]+)/([^/]+)/.*$','\1 :: \2') season,
         count(*) filter (where m.kind='Intro') intro,
         count(*) filter (where m.kind='Outro') outro
  from media_items i left join media_segments m on m.item_id = i.id
  where i.path like '%/TV/%' group by 1
) s where outro > 0 and intro = 0;
```

## Requirements on the fix

- **R-OBS-1**: `no_span` must remain distinguishable from `few_agreeing` and
  `low_confidence` after the change. A fix that makes everything `emitted` by
  loosening a gate would satisfy the counter and betray the contract.
- **R-OBS-2**: if a stage can now fail for a reason the current `Verdict` set
  cannot express, that instrumentation lands **first, as its own commit**, so it
  can ship and be read before the fix does.
- **R-OBS-3**: the per-season line must keep naming values, never classes. A
  reason that does not carry the number is another round of guessing.
- **R-OBS-4**: the signal is part of the contract — assert it in a unit test, and
  confirm the test fails with the instrumentation disarmed.
