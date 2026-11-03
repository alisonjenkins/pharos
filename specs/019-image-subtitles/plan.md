# Plan: image subtitles drawn by the native player

**Spec**: `spec.md` · **ADR**: 0045

## Stack and boundaries

- `pharos-transcode`: `libav/subtitle_images.rs` decodes one subtitle stream with `ffmpeg_the_third::decoder::Subtitle` (safe decode; the rect bitmap and palette are read through `AVSubtitleRect` pointers, as `attachment.rs` already reads codec params), converts each bitmap rect to RGBA PNG, and reports events as it goes. New worker op `TinyOp::SubtitleImages` / `LibavWorkerPool::subtitle_images`, crash-isolated like every libav op (V6).
- `pharos-cache`: `subtitle_image_cache.rs`, modelled on `trickplay_cache`: `{root}/{hash(path)}-{mtime}-{index}/` holding `manifest.json` and `{n}.png`, a `.complete` marker, a root `.gen_version` (`IMAGE_SUB_GEN_VERSION`), an LRU byte cap, per-key in-flight dedupe, extraction behind the shared bg-I/O gate.
- `pharos-server`: routes in `api/jellyfin/subtitles.rs` beside `stream.vtt`; `services/watch.rs` gives image tracks a manifest URL when the caller can draw them; `services/playback.rs` `Capabilities::profile()` declares the image codecs plus the text formats it already took as External; `rpc/library.rs` writes `image`.
- `pharos-api` / `pharos-client`: schema `image @2`, `imageSubtitles`, client mapping.
- `pharos-ui`: the web player declares the codecs; `watch_view.rs` draws an absolutely positioned layer inside the player sized to the video's content box; `app.rs` polls the manifest while incomplete and switches image tracks in place.

## Gates

TDD with disarm checks; ODD signal first (FR-007); `just test`, `just test-postgres` not needed (no SQL), `just compat-dioxus-full` with a real `dvd_subtitle` fixture built by `dvdauthor`'s `spumux` and remuxed with `-c:s copy`.
