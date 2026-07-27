# Quickstart: validating CMAF hardware affinity

## 0. Prerequisite gate — settle R4 FIRST

Nothing below matters if a hardware encoder is not self-consistent across
processes. On a host with the GPU:

```
# two independent segment encodes, same device, same settings
nix develop --command cargo run -p pharos-transcode --bin transcode_tool -- \
    segment --device nvenc:0 --input <src> --start 0    --dur 6 --out /tmp/a.m4s
nix develop --command cargo run -p pharos-transcode --bin transcode_tool -- \
    segment --device nvenc:0 --input <src> --start 600  --dur 6 --out /tmp/b.m4s
```

Extract and byte-compare the parameter sets (`avcC` extradata). **Identical → the
feature is viable. Different → stop; the CPU-only rule is correct and this spec
is dead.** Repeat once after restarting the worker process — a fresh process is
the case that matters.

## 1. Unit / integration (no GPU needed)

```
nix develop --command cargo nextest run -p pharos-transcode device
nix develop --command cargo nextest run -p pharos-transcode scheduler
```

Covers contract tests 1–4 with a synthetic device table: saturation does not
spill, cooldown errors rather than falls back, mpegts still balances, and a
cached segment from one pin is not served under another.

## 2. Full suite before pushing

```
nix develop --command just test
nix develop --command cargo clippy --workspace --lib --tests -- -D warnings
```

## 3. Verify in production, by query, not assertion

After deploy, play one title in a browser and run:

```
# SC-001 — where did the segments actually encode?
sum by (device) (pharos_segment_produced_total)

# the cost of the guarantee
sum by (outcome) (pharos_transcode_pin_total)

# SC-002 — encode time should fall to hardware levels
histogram_quantile(0.5, sum by (le) (rate(pharos_segment_transcode_seconds_bucket[5m])))
```

Baselines measured 2026-07-27 on the CPU-only path, for comparison:

| | before |
|---|---|
| jobs on cpu / Nvenc | 420 / 3 |
| median encode (6 s segment) | 3380 ms cpu · 1825 ms Nvenc |
| realtime ratio | 1.81× cpu · 3.3× Nvenc |

Log check for the decision itself:

```
{namespace="pharos"} |~ "rendition pinned|pin invalidated"
```

## 4. The real acceptance test

Three browsers, one title, SyncPlay group. Expect: encodes on the GPU, no
`outcome="shed"` on interactive jobs, and no `readiness gate timed out` in the
group's logs. That combination is what failed on 2026-07-27 and is the reason
this exists.

## 5. Rollback

`device_supports` regaining its `H264 && Fmp4 => false` arm restores today's
behaviour exactly. Bump `HLS_GEN_VERSION` again when rolling back, so
hardware-produced segments cannot be served under a CPU-produced init.
