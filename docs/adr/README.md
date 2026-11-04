# Architecture Decision Records

This directory records the **load-bearing decisions** behind pharos — the
choices that shaped the architecture and would be expensive or contentious to
reverse. Each record captures the context at the time, the decision, and its
consequences, so a future reader (or the person who made it) can reconstruct
*why* without archaeology through git history.

## Format

Lightweight [Nygard-style](https://cognitect.com/blog/2011/11/15/documenting-architecture-decisions)
ADRs. One file per decision, `NNNN-kebab-title.md`, monotonically numbered.
Copy [`0000-template.md`](0000-template.md) to start a new one.

Sections: **Status**, **Context**, **Decision**, **Consequences** (and
optional **Alternatives considered** / **References**).

## Status values

- **Accepted** — in force.
- **Superseded by ADR-NNNN** — replaced; kept for history (never delete an ADR).
- **Proposed** — under discussion, not yet in force.
- **Deprecated** — no longer relevant, but not actively replaced.

## Conventions

- ADRs are **immutable once Accepted**. To change a decision, write a new ADR
  that supersedes the old one and update both `Status` lines. This preserves
  the decision timeline.
- Keep them short. An ADR is the *why*, not a design doc — link to `SPEC.md`,
  `docs/architecture.md`, or code for the *how*.
- Dates are ISO-8601 UTC.

> **Backfill note.** ADRs 0001–00xx were written retroactively (2026-07) to
> capture decisions already embodied in the codebase, so pharos has a reviewable
> decision log going forward. Their dates reflect *when documented*, not
> necessarily when first decided; where the original decision date is known it
> is noted in-text. ADRs 0020–0027 were backfilled the same way on 2026-10-05
> from specs 003–008 and the code; each marks rationale it could not find as
> "not recorded".

## Index

| # | Title | Status |
|---|-------|--------|
| [0001](0001-jellyfin-api-compatibility.md) | Jellyfin API as the client contract | Accepted |
| [0002](0002-rust-actix-sqlx-stack.md) | Rust / actix-web / sqlx / tokio stack | Accepted |
| [0003](0003-sqlite-default-store.md) | SQLite (WAL) default store, Postgres alternative | Accepted (amended by 0015) |
| [0004](0004-ffmpeg-libav-default-backend.md) | libav in-process default backend + crash-isolated worker pool | Accepted |
| [0005](0005-per-segment-hls-transcode.md) | Per-segment on-demand HLS transcode + VP9-in-fMP4 for Firefox | Accepted |
| [0006](0006-subtitles-out-of-band.md) | Subtitles delivered out-of-band, never muxed | Accepted |
| [0007](0007-trickplay-pregeneration.md) | Trickplay pre-generation with playback-yield gate | Accepted (gate superseded by 0017) |
| [0008](0008-incremental-scan-signature.md) | Incremental scan by (mtime, size) signature | Accepted |
| [0009](0009-nix-flake-reproducibility.md) | Nix flake + devShell; buildRustPackage for the OCI image | Accepted |
| [0010](0010-cd-ghcr-flux-automation.md) | CI on self-hosted builder → GHCR → Flux image automation | Accepted |
| [0011](0011-auth-hashed-bounded-tokens.md) | Hashed, bounded-lifetime session tokens + CLI admin bootstrap | Accepted |
| [0012](0012-dioxus-wasm-ui.md) | Dioxus + dx (WASM) for the built-in web UI | Accepted; serving superseded by 0033, then 0038 |
| [0013](0013-observability-tracing-metrics.md) | Structured tracing + Prometheus metrics + OTLP traces | Accepted |
| [0014](0014-watch-together-group-sync.md) | Watch-together group-sync over a WebSocket, Jellyfin SyncPlay-compatible | Accepted (extended by 0016) |
| [0015](0015-postgres-cnpg-deployment.md) | CNPG Postgres for the home deployment; zero-downtime rolling deploys | Accepted |
| [0016](0016-syncplay-durability-distribution.md) | Durable, multi-replica SyncPlay (persisted groups + advisory-lock ownership) | Accepted |
| [0017](0017-adaptive-background-io-gate.md) | Adaptive background-I/O gate (shared semaphore, parks while streaming) | Accepted |
| [0018](0018-intro-outro-detection.md) | Automatic intro / outro detection (audio fingerprinting) | Accepted |
| [0019](0019-vr-playback-vendored-client-script.md) | VR playback via a vendored client script in the jellyfin-web bundle | Accepted |
| [0020](0020-cmaf-hw-affinity.md) | CMAF HLS renditions with per-device hardware affinity | Accepted |
| [0021](0021-self-tuning-playback-admission.md) | Self-tuning (AIMD) playback admission | Accepted |
| [0022](0022-transcode-device-spread.md) | Weighted spread of transcodes across CPU and GPU devices | Accepted |
| [0023](0023-remote-sources.md) | Remote media sources resolved on demand | Accepted |
| [0024](0024-books-library.md) | Books as a first-class library kind (client-side readers, no ffmpeg) | Accepted |
| [0025](0025-kindle-conversion.md) | Kindle-format book conversion (boko, EPUB on delivery, derived state) | Accepted |
| [0026](0026-client-capability-negotiation.md) | Playback decided by negotiating client against server capabilities | Accepted |
| [0027](0027-unified-seek-model.md) | One seek model for DirectPlay and HLS | Accepted |
| [0028](0028-native-api-for-first-party-clients.md) | pharos-ui and future pharos apps use a pharos-native (Cap'n Proto) API; Jellyfin API stays for third-party clients | Accepted |
| [0029](0029-native-api-transport-and-session.md) | Native API transport: per-worker Cap'n Proto RPC over a WebSocket, sessions resumed from a cookie or device token, off by default | Accepted |
| [0030](0030-edition-full-probe.md) | Each grouped edition keeps its full probe in `alternate_sources_json` | Accepted |
| [0031](0031-item-file-auth-cookie.md) | Item file routes require auth; the `JellyfinAuth` cookie authenticates only GET/HEAD byte routes | Superseded in part by 0040 |
| [0032](0032-syncplay-members-record-their-user.md) | SyncPlay group members record their user (persisted and on the bus, absent = sees nothing); a queue change needs every member to see it | Accepted |
| [0033](0033-pharos-ui-served-at-the-root-from-the-pharos-image.md) | pharos-ui is baked into the pharos image and served at `/` (SPA fallback for browsers only, `/ui/*` → 308); the sidecar is off by default | Superseded by 0038 |
| [0034](0034-native-ui-media-cookie.md) | The native web UI authenticates element fetches with a `pharos_media` cookie (same token as `pharos_session`, `JellyfinAuth`'s V191 scope) | Accepted |
| [0035](0035-native-ui-vendors-hlsjs.md) | The native web UI plays HLS through a vendored, pinned hls.js 1.7.4 served from `'self'`, loaded only where the browser lacks native HLS | Accepted |
| [0036](0036-themes-are-toml-over-a-token-model.md) | Themes are TOML documents over a typed, platform-neutral token model; the web UI renders them as `--px-*` custom properties and `pharos.css` reads nothing else | Accepted |
| [0037](0037-per-user-prefs-store-and-telemetry-opt-in-gate.md) | Per-user prefs live in their own `UserPrefStore` (opaque bytes per key, policy in `rpc/prefs.rs`); `Telemetry.submit` drops batches from users who have not opted in (`diagnostics.opt_in`) | Accepted |
| [0038](0038-pharos-ui-served-under-ui.md) | pharos-ui is served under `/ui/` (`/ui` → 308 `/ui/`), leaving `/` to the core root and jellyfin-web; supersedes 0033's root mount | Accepted |
| [0039](0039-gpu-layer-webgl2-via-glow.md) | The native UI draws GPU effects with WebGL2 via `glow` behind a `Surface` port: compiled-in shader catalogue, bounded uniforms, CSS fallback for reduced motion, no WebGL2 or a lost context | Superseded in part by [0049](0049-gpu-layer-calls-webgl2-directly-and-restores-lost-contexts.md) |
| [0040](0040-item-images-anonymous-for-native-apps.md) | Item images answer anonymous requests again (native apps' image loaders send no credential); a resolving credential still applies the library policy; supersedes 0031's image decision | Accepted |
| [0041](0041-server-is-the-cast-sender.md) | pharos is the Cast sender (hand-rolled CASTV2 in `pharos-cast`); a TV joins a SyncPlay group as a non-gating `CastSeat`, offered to clients as a virtual session, TVs declared in config | Accepted |
| [0042](0042-token-scopes.md) | Access tokens carry a scope (`Full`, `Playback`); a TV's token is playback-only on a deny-by-default route allowlist, device-bound, short-lived and revoked with the cast | Accepted |
| [0043](0043-mvc-transcode-is-2d.md) | A transcode of Blu-ray 3D (H.264 MVC) is 2D because ffmpeg has no MVC decoder; `Output3D` is the one answer to what a transcode does to a 3D source, and PlaybackInfo advertises no 3D format for a flattened delivery | Accepted |
| [0044](0044-subtitle-burn-is-drawn-per-eye-on-packed-3d.md) | A subtitle burned into packed 3D (side by side, top and bottom) is drawn into each eye with the layout kept; `Mono2D` only on a client or profile request, and the segment cache keys a per-eye burn apart from unburned and `_mono2d` segments; supersedes V183's burn rule with V234 | Accepted |
| [0045](0045-native-group-shares-the-syncplay-routing-path.md) | The native `Group` interface drives the same SyncPlay actors as the Jellyfin handlers through `group_ops::route_command`; member key `{userId}:{device}`, push via a `GroupSink` callback, dropping a `Membership` is a socket loss with grace, the client owns drift and server commands win | Accepted |
| [0046](0046-lyrics-sources-and-lofty.md) | Song lyrics come from one service shared by the native and Jellyfin wires: `.lrc` sidecar, the file's tags (read with `lofty`, `SYLT` first), `.txt` sidecar, then an opt-in online provider (LRCLIB) behind `OnlineLyrics` | Accepted |
| [0047](0047-book-reader-is-a-standalone-rust-crate.md) | The book reader is two pharos-free crates (`quire` parses and sanitises, `quire-web` mounts into a shadow root and paginates with CSS columns); the browser lays out; positions are a locator string stored apart from `PlaybackPositionTicks`; PDF engine decided by a spike | Proposed |
| [0048](0048-dynamic-mixes-are-stateless-server-picks.md) | Dynamic music mixes are picked by a pure server engine behind one stateless `Library.mix(spec, recent, count)`; the client holds the rules; hard rules never bend, other bends are named and counted; saved mixes are one capnp `SavedMixes` Prefs value | Accepted |
| [0049](0049-gpu-layer-calls-webgl2-directly-and-restores-lost-contexts.md) | The GPU layer calls WebGL2 through `web_sys` directly (no `glow`, which asks for every extension) and restores a lost context, falling back only after 5 s; supersedes ADR-0039 in part | Accepted |
| [0050](0050-song-tempo-and-energy-from-a-background-window-analysis.md) | Song tempo and energy come from the BPM tag and a pure-Rust analysis of a 30 s mono 11 kHz window, in one libav worker op run by a one-at-a-time background sweep; stored in `media_items` columns `put` never writes, valid while the file signature matches | Accepted |
| [0051](0051-user-avatars-are-re-encoded-and-kept-as-a-user-pref.md) | A user avatar is decoded with the `image` crate under size, dimension and allocation caps, cropped to 512 px and re-encoded (PNG if transparent, else JPEG), stored under the `avatar` user pref, and served on Jellyfin's `/UserImage` and the native `Session` alike | Accepted |
| [0052](0052-one-player-on-a-stage-beside-the-routes.md) | The native UI mounts one player on a stage beside the routed screen; the watch route sets its item and full view, other routes shrink it to a floating window or mini bar by class, and a watch navigation while compact plays in place | Accepted |
| [0053](0053-songs-hand-over-on-the-stage.md) | A song hands the stage over to the next: the old player stays mounted (renamed `pharos-video-out`, reporting stopped) until the next is negotiated, so nothing flashes; an opt-in per-device crossfade overlaps the two by stepping their volumes | Accepted |
| [0054](0054-group-members-see-each-others-watched-state.md) | A member of a watch-together group can read the other members' played state (played, resume percent, episode counts) through `Membership.seen`: only peers in the caller's group, only items the caller may see (V201), at most 200 ids, counted in `pharos_group_seen_total`; joining is the consent | Accepted |
| [0055](0055-group-tracks-are-advisory-client-applied.md) | A watch-together group's audio and subtitle choice is advisory group state: the server records and announces it (and gives it to a late joiner, clears it on a queue change) but never picks a stream by it; each following client applies it to its own player and a "Just me" member ignores it | Accepted |
| [0056](0056-the-ui-lowers-playback-quality-by-renegotiating.md) | The native UI lowers video quality itself: on repeated stalls or a link under 1.3x the stream it renegotiates with the next lower 8/4/2/1 Mbps cap at the current position, steps back up after 60 s of 2x headroom, and treats the manual maximum as a ceiling; the browser master has one rung, so hls.js ABR cannot do it |
