# 008-remote-sources — play a URL as if it were a library file

**Status**: designed 2026-08-01
**Depends on**: 007 (`pharos_cache::generation()` and the client-visible `g=`,
whose limits this spec has to work around), the HLS transcode path generally.
**New ids**: V132–, B179–.

## The goal, and what it turned out to require

Sit down with friends and watch anything together, YouTube included.

Investigation found the group-watch half already solved and the missing half
narrower than it looked.

**SyncPlay needs zero changes.** `QueueEntry { item_id: String,
playlist_item_id: String }` (`crates/pharos-sync/src/group.rs:603`) — the group
actor never resolves an id against the database. There is no `MediaSource`, no
duration and no repository lookup anywhere in `group.rs`'s 6,989 lines; it
forwards opaque strings and timestamps, and each client fetches its own
`PlaybackInfo`. **Anything playable is group-watchable for free.**

**Stock jellyfin-web needs zero changes.** A URL-backed item advertised with
`supports_direct_play: false` and an HLS `transcoding_url` is fetched through
hls.js exactly like a library file.

So this is a server-only spec, and its whole outcome is: paste a URL, it becomes
a library item, it plays, and the SyncPlay that already exists carries the group.

`crates/pharos-ui` is deliberately out of scope — see the last section.

## What already works, unmodified

Three facts do most of the work, and none of them needed changing.

**The HLS path never touches the filesystem for media bytes.** It forwards
`item.path` as an opaque string into ffmpeg: `hls.rs:472`
`sched.probe(&item.path)`, `hls.rs:1193` `submit_live(item.path.clone(), ..)`,
`hls.rs:1216` `transcode(&item.path, ..)`.

**ffmpeg already accepts a URL.** libav input is
`ffmpeg::format::input<P: AsRef<OsStr>>(path_or_url)`, which goes straight to
`avformat_open_input`, so libavformat's protocol layer resolves `https://`
itself. The spawn backend does `a.push("-i".into()); a.push(input.to_string())`
(`crates/pharos-transcode/src/lib.rs:644`). nixpkgs `ffmpeg-headless` builds with
gnutls, so the https protocol is compiled in. Neither backend needs a change to
read a remote source.

**`Path::starts_with` is component-wise**, so a synthetic `ytdlp://…` path
satisfies `root_like_pattern` (`pharos-store-sqlx/src/lib.rs:123`), library
assignment (`sqlite.rs:2939`) and `restrict_to_parent` (`items.rs:5131`) with no
special-casing. The item appears in its CollectionFolder by the ordinary
mechanism.

## The design

### Resolution, not download

`yt-dlp -j <url>` returns metadata plus per-format `url` fields that are
HTTP-range-capable. At play time the resolver hands ffmpeg those media URLs
directly — `-i <video_url> -i <audio_url>` with `-map 0:v:0 -map 1:a:0` for
YouTube's DASH split — so `-ss` becomes an HTTP range request and seeking is
O(1). Resolutions are cached with a short TTL. The stored path is a **stable
synthetic** `ytdlp://<extractor>/<video_id>`, never a resolved URL, because
`media_items.path` is `TEXT NOT NULL UNIQUE`
(`migrations/sqlite/0001_init.sql:3`) and a signed URL rotates.

**A sequential download-and-gate was designed and rejected.** It cannot work.
For `bestvideo+bestaudio` yt-dlp writes two `.part` files and then runs an ffmpeg
**merge**, so the output path does not exist until everything has finished —
there is no growing file to watch, and any "bytes available" gate degenerates
into "wait for the whole download" while costing all of its complexity.
`--no-part` fixes the rename, not the merge. Forcing a progressive format caps
YouTube at 360p and still leaves `moov` at EOF, which makes `avformat_open_input`
**fail outright** on a partial file rather than short-read. And `-N` concurrent
fragments arrive out of order, so bytes-available stops being a contiguous
prefix — the arithmetic premise dies too.

### V132 — a cached artefact is invalidated everywhere it was published, or not at all

The segment cache is not the only cache. Segments and init ship
`Cache-Control: public, max-age=31536000, immutable` (`hls.rs:1167`,
`hls.rs:1200`), so bytes that changed behind a stable id survive in the
**browser** for a year, beyond any server-side wipe's reach. A per-item source
generation therefore has to appear in four places, and three of them is a bug:

1. `SegmentIdentity` (`hls_cache.rs:133`) — the server disk key,
2. `rendition_qs` (`hls.rs:1644`) — as `s=<srcgen>`, beside 007's `g=`,
3. `segment_etag` (`hls.rs:1230`) — whose doc comment already records that every
   byte-affecting dimension must be read off one struct or it silently drifts,
4. the subtitle and waveform cache keys, replacing mtime.

It **cannot** ride `pharos_cache::generation()`. That is a process-wide
`OnceLock` fixed at boot (`hls_cache.rs:1281`); bumping it because one remote
video re-resolved would wipe every local title's segments. 007's generation
answers "did the server's placement rule change" — a per-process question. This
one answers "did the bytes behind this id change" — a per-item question. They are
different questions and need different mechanisms.

### V133 — a cache key derived from a file's existence lies about a source that has none

`mtime_secs` returns **0 for any nonexistent path**
(`subtitle_cache.rs:486-489`), silently and by design. Every remote item would
therefore key its burnt-subtitle and waveform cache at mtime 0 — identical across
every remote item, and permanently stale across re-resolution, with no error
anywhere to say so. Ten sites: `hls.rs:1518,2763`,
`subtitles.rs:220,269,457,597,810,944,1536`, `waveform.rs:101`. Remote origins
use the source generation instead.

### V134 — a background sweep that cannot succeed on an item must decline it, not retry it

`trickplay_backfill.rs:346` calls `ensure_generated_all(.., &item.path)`, which
fails on an unknown protocol, so `is_generated` never becomes true and the item
is retried every `PASS_INTERVAL` **forever**, holding a `BgPermit` each pass
(`trickplay_backfill.rs:263-296`). The same shape sits at
`trickplay_backfill.rs:398,429`, `segment_backfill.rs:391,455` (fingerprint does
`File::open` → ENOENT) and `metadata_backfill.rs:2678`.

This is a **ship-order dependency**, not a tidy-up: the fix must land before any
commit can insert a remote row, or the first insert starts an unbounded retry
loop that competes with playback for the background-I/O gate.

### Origin is a type, not a string test

`MediaItem.path` stays a `PathBuf`. An accessor returns
`Origin::Local(LocalPath) | Origin::Remote(RemoteRef)`, where `LocalPath` is
**unforgeable** in the manner of `BgPermit` (`bg_io.rs:20` — "its VALUE is being
un-forgeable"), and every filesystem-only helper takes `LocalPath`. A remote item
then cannot reach an fs call without the compiler objecting.

A bare enum that call sites must remember to match on would be the string-sniffing
bug wearing a type. The risk is not on the playback path, which this spec
inspects deliberately; it is in the background sweeps above, which take a
`&Path` out of a struct field and where nobody will think to add a branch.

### V135 — a network fetch does not spend the disk budget

`bg_io`'s whole design is that minting a permit forces a choice
(`bg_io.rs:8-19`). Remote fetches are network, not disk: taking
`BgPermit::acquire` occupies a gate they do not contend for, and marking them
`playback_priority` leaves a background prefetch unmetered. "Network" is a choice
the module cannot currently express, so it gains one.

### Byte-range read-through cache

A read-through cache addressed by **byte range**, not sequential fill, served to
ffmpeg over a local `http://127.0.0.1:…/src/<ref>` URL. A seek past the fill
point *fetches*; it never waits. Without it every segment encode re-opens an
HTTPS connection to googlevideo and invites throttling. Measurement tunes its
window and eviction policy; it does not decide whether it ships.

### Forcing the transcode path

`stream.rs` is built on `actix_files::NamedFile::open_async(&item.path)`
(`:152`, `:863`) and `tokio::fs::metadata` (`:798,942,962,989`), so direct play
is unavailable for a remote item by construction. It is refused explicitly rather
than left to fail: a new `DirectPlayBlock::RemoteSource`
(`device_profile.rs:369`) and `supports_direct_play: false`. Both that enum and
`pharos_source_unreadable_total{reason}` are dashboard contracts, so each gains
an arm and the distinct-label tests are updated — a by-design refusal must not
land in the bucket an alert reads as "media is missing".

### Ingestion

`POST /Pharos/Remote/Items` takes a URL, resolves metadata, creates the
`libraries` row and inserts with `library_id` stamped **directly** (template:
`library_watch.rs:555`). It must never route through `POST
/Library/VirtualFolders`, which spawns a scan for any path outside an existing
root (`items.rs:6598-6607`) and would kick a scan of `ytdlp://`. It returns the
item id so a client can immediately `SetNewQueue`.

Two traps in the `MediaProbe` it builds:

- `yt-dlp -j` reports `vcodec` as `"avc1.640028"`, but `MediaProbe` needs
  `video_profile` ("High") and `video_level` (×10) as separate fields to build
  the RFC 6381 CODECS attribute (`lib.rs:1706-1713`, consumed at
  `hls.rs:443-447`). They must be parsed back out or the master playlist
  advertises the wrong CODECS and Safari refuses the variant.
- `duration_ms` must always be persisted, or the `hls.rs:472` fallback probe
  fires and the playlist renders as 0 s.

### V136 — a synthetic item is never parked under a directory a scan walks

`sweep_unseen` is safe here only by accident of arithmetic: a walk of a
nonexistent root yields a walkdir error, `walk_errors == 1`, and the sweep is
skipped (`fs.rs:869`). Park remote items under a **real** scan root instead and
the walk succeeds, finds none of them, and deletes every remote row. B98's
blast-radius guard does not save it — that needs ≥100 deletions **and** >25%
(`pharos-store-sqlx/src/lib.rs:146-158`), so a library of 40 videos is wiped
without so much as a warning line.

## Signals

Named before the code, per §ODD.

- `pharos_remote_resolve_total{extractor,outcome}` — every resolution attempt and
  its verdict, carrying the extractor so one failing site is distinguishable from
  a broken resolver.
- `pharos_remote_resolve_seconds` — resolution latency; a slow resolve is
  indistinguishable from a hung one without it.
- `pharos_remote_range_fetch_total{outcome}` and
  `pharos_remote_range_bytes_total` — whether the range cache is doing its job,
  and the throttling signal that would say it is not.
- Existing and already sufficient: `pharos_segment_produced_total{outcome,reason}`,
  `pharos_playback_decision_total{direct_play_block}`,
  `pharos_source_unreadable_total{reason}`.

The probe at `hls.rs:472` becomes a **network** call issued inside the libav
worker pool. It carries a timeout, or one dead URL parks a worker.

## Verification

- **Unit**: the `Origin` parser; `vcodec` → profile/level; the resolver against
  captured `yt-dlp -j` JSON; source-generation propagation into all four sites of
  V132.
- **Regression, each must fail without its fix**: a remote item under a real scan
  root is not swept (V136); a re-resolved source serves neither stale disk
  segments nor a stale `immutable` browser entry (V132); a background sweep
  declines a remote item instead of retrying it (V134).
- **Integration**: a local HTTP server serving a fixture, resolved through a stub
  resolver, driven end to end through `/videos/{id}/master.m3u8`.
- **Live, by query not assertion**: play a URL in stock jellyfin-web, then
  group-watch it with two browsers via `just compat-syncplay`. Read
  `pharos_remote_resolve_total{outcome}`,
  `pharos_segment_produced_total{outcome,reason}` and interactive
  `queue_wait_seconds` — a remote source must not regress local playback.

## Out of scope

`crates/pharos-ui` gets **spec 009**. It does not block this work, but it is a
real gap and a larger one: `player.rs` is a bare `<video src=…>` with no
MSE/hls.js anywhere in the crate, so on desktop Chrome and Firefox it cannot play
*any* transcoded item; and `views/group.rs` is a 155-line presentational stub
that is never instantiated, with no websocket anywhere in the crate, so it cannot
SyncPlay at all.

`yt-dlp` is not currently in `flake.nix` and must be added to the devShell and
the OCI image; host tooling is not relied on.

Downloading or restreaming YouTube content is contrary to YouTube's Terms of
Service. `[remote].enabled` defaults to **false**.
