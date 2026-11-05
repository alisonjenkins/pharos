# Plan: image subtitles drawn by the native player

**Spec**: `spec.md` · **ADR**: 0060

## Stack and boundaries

- `pharos-transcode`: `libav/subtitle_images.rs` decodes one subtitle stream with `ffmpeg_the_third::decoder::Subtitle` (safe decode; the rect bitmap and palette are read through `AVSubtitleRect` pointers, as `attachment.rs` already reads codec params), converts each bitmap rect to RGBA PNG, and reports events as it goes. New worker op `TinyOp::SubtitleImages` / `LibavWorkerPool::subtitle_images`, crash-isolated like every libav op (V6).
- `pharos-cache`: `SubtitleCache::image_subtitles` in `subtitle_cache.rs`, beside the event-window scan it resembles: `{root}/subtitles/{hash(path)}-{mtime}-{index}-i{IMAGE_SUB_GEN_VERSION}/` holding `manifest.json` and `{n}.png`. The manifest's own `complete` flag marks a finished extraction and the version is in the directory name, so neither needs a marker file. Per-key in-flight dedupe and a per-boot failure memo share the window scan's map; extraction runs behind the shared bg-I/O gate.

## Known gaps

- The image directories have no byte cap or eviction yet. A DVD track is a few MB, so this grows only with the number of image tracks actually watched.
- A track whose extraction failed answers 404 for the rest of the run, and the player shows nothing for it; the negotiation does not yet know about the failure, so it is not burned instead. The `failed` outcome of `pharos_subtitle_image_extract_total` shows when this happens.
- No PGS fixture: nixpkgs has no PGS authoring tool, so `hdmv_pgs_subtitle` decoding is covered only by libav's shared subtitle API and the DVD fixture.
- `pharos-server`: routes in `api/jellyfin/subtitles.rs` beside `stream.vtt`; `services/watch.rs` gives image tracks a manifest URL when the caller can draw them; `services/playback.rs` `Capabilities::profile()` declares the image codecs plus the text formats it already took as External; `rpc/library.rs` writes `image`.
- `pharos-api` / `pharos-client`: schema `image @2`, `imageSubtitles`, client mapping.
- `pharos-ui`: the web player declares the codecs; `watch_view.rs` draws an absolutely positioned layer inside the player sized to the video's content box; the overlay polls the manifest while incomplete, and `app.rs` switches image tracks in place.

## Gates

TDD with disarm checks; ODD signal first (FR-007); `just test`, `just test-postgres` not needed (no SQL), `just compat-dioxus-full` with a real `dvd_subtitle` fixture built by `dvdauthor`'s `spumux` and remuxed with `-c:s copy`.
