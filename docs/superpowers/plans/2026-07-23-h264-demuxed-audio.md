# h264 Demuxed-Audio HLS Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give browser h264 HLS clients a demuxed-audio CMAF rung (video-only h264 fMP4 + a shared separate audio rendition) so an audio-track switch reuses cached video instead of cold-re-transcoding it — eliminating the SyncPlay group thrash.

**Architecture:** Mirror the in-prod VP9 fMP4 demuxed-audio surface for h264. Parameterize the fMP4 video-surface internals by output video codec (VP9 handlers keep working, unchanged behaviour), add `/videos/{id}/h264cmaf/*` routes that reuse those internals with H264, and route *browser* h264 masters to a video-only h264-fMP4 rung + the existing demuxed audio group. Native / no-`renditions` clients keep the muxed mpegts path byte-for-byte.

**Tech Stack:** Rust, actix-web, `pharos-transcode` (`SegmentOpts`/`SegmentContainer`/`SegmentVideo`/`SegmentAudio`), `pharos-cache` (`hls_cache`), fMP4 post-processing (`fmp4::process_segment`).

## Global Constraints

- Dev only inside the Nix devShell: `nix develop --command <cmd>`.
- Tests via `cargo nextest run`; doctests via `cargo test --doc`.
- Clippy is `-D warnings` with `unwrap_used` / `expect_used` DENY in non-test code — use `?`/`ok_or_else`, never `.unwrap()`/`.expect()` outside `#[cfg(test)]`.
- MSRV 1.80 idioms (no `repeat_n`, etc.).
- Both ffmpeg backends must build: default (`ffmpeg-lib`) AND `--no-default-features --features ffmpeg-spawn`.
- Host git uses difftastic: diff with `git diff --no-ext-diff`.
- **The muxed mpegts h264 path and the VP9 path must remain behaviourally unchanged** — only NEW code (h264-cmaf) and a browser-gated master branch are added. A native (no-`renditions`, non-Mozilla-UA) master must stay byte-identical to today.
- **The h264-cmaf video segment MUST be audio-independent**: `audio: None`, `audio_source_stream_index: None`, so its cache key never varies by `AudioStreamIndex` (this is the entire point).
- All-fMP4 master only: a demuxed h264 master carries a video-only h264-fMP4 rung + the fMP4 audio group and NO muxed mpegts rung. Never mix muxed and demuxed audio models in one master (`hls.rs:537` — the mixed-codec buffering outage).
- Reuse the EXISTING audio rendition endpoint `/videos/{id}/vp9/audio.m3u8` (codec-neutral Opus-in-fMP4, already keyed on `AudioStreamIndex`). Do NOT add a second audio codec.

---

## File Structure

- `crates/pharos-server/src/api/jellyfin/hls.rs` — all changes: parameterize fMP4 internals, add h264-cmaf handlers + routes, add the browser-gated demuxed-h264 master rung + `is_web_client` gate, unit tests.
- `crates/pharos-server/tests/jellyfin_hls_multivariant.rs` — master-structure + segment integration tests.
- `crates/pharos-server/tests/client_compat.rs` — end-to-end demuxed-h264 flow (optional extension).

No new files, no new crates, no dependency changes.

---

### Task 1: Parameterize the fMP4 video surface by output codec

Make the VP9 fMP4 segment-opts + segment-raw internals take the output video codec, so h264-cmaf can reuse them. VP9 behaviour is unchanged (it passes `Vp9`).

**Files:**
- Modify: `crates/pharos-server/src/api/jellyfin/hls.rs` (`vp9_segment_opts` ~2342, `vp9_segment_raw` ~2451, their callers `vp9_init` ~1748, `vp9_segment` ~1788)

**Interfaces:**
- Produces: `fmp4_segment_opts(state, req, item, seg, video: SegmentVideo, audio_stream_index, subtitle_stream_index) -> SegmentOpts` (video-only, `audio: None`, `container: Fmp4`, `video: Some(video)`); `vp9_segment_opts` becomes a thin caller passing `SegmentVideo::Vp9`.

- [ ] **Step 1: Add a failing test for h264 fMP4 segment opts**

Add to the `#[cfg(test)] mod tests` in `hls.rs` (near `vp9_segment_opts_burns_text_sub_and_flags_is_text` ~3318). This mirrors the existing vp9 opts test shape; reuse its `AppState`/`req`/`item` construction helper (copy the setup from that test verbatim — do not invent a new fixture).

```rust
#[actix_web::test]
async fn h264_cmaf_segment_opts_is_audio_free_h264_fmp4() {
    // The whole point: the h264-cmaf video segment carries NO audio, so its
    // cache key never varies by AudioStreamIndex.
    let (state, req, item) = h264_cmaf_opts_fixture().await; // reuse the vp9 test's setup
    let opts = fmp4_segment_opts(&state, &req, &item, 0, SegmentVideo::H264, Some(1), None).await;
    assert_eq!(opts.container, SegmentContainer::Fmp4);
    assert_eq!(opts.video, Some(SegmentVideo::H264));
    assert_eq!(opts.audio, None, "video segment must be audio-free");
    assert_eq!(opts.audio_source_stream_index, None);
    // Same call with a DIFFERENT audio index yields identical audio-affecting fields.
    let opts2 = fmp4_segment_opts(&state, &req, &item, 0, SegmentVideo::H264, Some(2), None).await;
    assert_eq!(opts2.audio, None);
    assert_eq!(opts2.audio_source_stream_index, None);
}
```

- [ ] **Step 2: Run it — expect a compile failure (`fmp4_segment_opts` not found)**

Run: `nix develop --command cargo test -p pharos-server --lib h264_cmaf_segment_opts_is_audio_free_h264_fmp4 2>&1 | tail`
Expected: compile error, unresolved `fmp4_segment_opts`.

- [ ] **Step 3: Extract `fmp4_segment_opts` from `vp9_segment_opts`**

Rename the body of `vp9_segment_opts` to `fmp4_segment_opts` with an added `video: SegmentVideo` parameter, and set `video: Some(video)` in the returned `SegmentOpts` (was hardcoded `Some(SegmentVideo::Vp9)`). Everything else (frame-aligned range, bitrate cap, subtitle handling, `audio: None`, `audio_source_stream_index: None`) is unchanged.

```rust
async fn fmp4_segment_opts(
    state: &AppState,
    req: &HttpRequest,
    item: &pharos_core::MediaItem,
    seg: u32,
    video: SegmentVideo,
    _audio_stream_index: Option<u32>,
    subtitle_stream_index: Option<u32>,
) -> SegmentOpts {
    // ... identical body to today's vp9_segment_opts ...
    SegmentOpts {
        container: SegmentContainer::Fmp4,
        video: Some(video),
        audio: None,
        video_bitrate_bps: Some(bitrate),
        audio_bitrate_bps: None,
        start_position_ticks: start_ticks,
        duration_ticks: Some(duration_ticks),
        audio_source_stream_index: None,
        burn_subtitle_stream_index: sub_rel,
        burn_subtitle_is_text: sub_is_text,
        burn_subtitle_ass_path: None,
        burn_fonts_dir: None,
    }
}

/// VP9 video-only fMP4 opts (unchanged behaviour) — thin wrapper.
async fn vp9_segment_opts(
    state: &AppState,
    req: &HttpRequest,
    item: &pharos_core::MediaItem,
    seg: u32,
    audio_stream_index: Option<u32>,
    subtitle_stream_index: Option<u32>,
) -> SegmentOpts {
    fmp4_segment_opts(state, req, item, seg, SegmentVideo::Vp9, audio_stream_index, subtitle_stream_index).await
}
```

Add the test fixture helper `h264_cmaf_opts_fixture` by copying the exact `AppState`/`HttpRequest`/`MediaItem` construction the existing `vp9_segment_opts_burns_text_sub_and_flags_is_text` test uses (extract it into a shared helper if that test builds them inline, and call it from both).

- [ ] **Step 4: Run the new test + the existing VP9 opts test**

Run: `nix develop --command cargo test -p pharos-server --lib fmp4_segment_opts h264_cmaf_segment_opts vp9_segment_opts 2>&1 | tail -20`
Expected: PASS (both the new h264 test and the unchanged vp9 test).

- [ ] **Step 5: Commit**

```bash
git add crates/pharos-server/src/api/jellyfin/hls.rs
git commit -m "refactor(hls): parameterize fMP4 segment opts by output video codec"
```

---

### Task 2: h264-CMAF video surface — playlist, init, segment routes

Add `/videos/{id}/h264cmaf/main.m3u8`, `/h264cmaf/init.mp4`, `/h264cmaf/{seg}.m4s`, reusing the VP9 fMP4 internals with `SegmentVideo::H264`. Audio comes from the existing `/vp9/audio.m3u8` (added to the master in Task 3).

**Files:**
- Modify: `crates/pharos-server/src/api/jellyfin/hls.rs` (route registration ~37-95; add handlers near the vp9 ones ~1699-1830)

**Interfaces:**
- Consumes: `fmp4_segment_opts(..., SegmentVideo::H264, ...)` (Task 1); `vp9_segment_raw` (codec-agnostic — takes `SegmentOpts`); `fmp4::process_segment`; `segment_time_range`, `playback_qs`, `load_hls_item`, `fetch_item`, `check_session`, `gate_image_sub_burn`, `resolve_text_burn_assets` (all existing).
- Produces: routes `h264cmaf_variant`, `h264cmaf_init`, `h264cmaf_segment`.

- [ ] **Step 1: Write a failing test for the h264-cmaf media playlist**

Add to `hls.rs` tests. Reuse the `master_body`/token setup pattern from `master_playlist_uses_real_resolution_and_bitrate_from_probe` (~2826) for building an authed `TestRequest` against a seeded item id 9.

```rust
#[actix_web::test]
async fn h264cmaf_main_playlist_is_fmp4_video_only() {
    let (app, token) = hls_test_app().await; // reuse existing test app builder
    let uri = format!("/videos/9/h264cmaf/main.m3u8?api_key={token}");
    let req = test::TestRequest::get().uri(&uri).to_request();
    let body = String::from_utf8(test::call_and_read_body(&app, req).await.to_vec()).unwrap();
    assert!(body.contains("#EXT-X-VERSION:7"));
    assert!(body.contains("#EXT-X-MAP:URI=\"/videos/9/h264cmaf/init.mp4"), "declares fMP4 init");
    assert!(body.contains("/videos/9/h264cmaf/0.m4s"), "points at h264cmaf .m4s segments");
    assert!(!body.contains(".ts"), "video-only fMP4, no mpegts");
}
```

- [ ] **Step 2: Run it — expect 404 (route not registered)**

Run: `nix develop --command cargo test -p pharos-server --lib h264cmaf_main_playlist_is_fmp4_video_only 2>&1 | tail`
Expected: FAIL (404 / assertion — route missing).

- [ ] **Step 3: Add the three handlers**

Mirror `vp9_variant`/`vp9_init`/`vp9_segment` exactly, swapping the URL prefix `/vp9/` → `/h264cmaf/` and passing `SegmentVideo::H264` to `fmp4_segment_opts`. `vp9_segment_raw` is reused unchanged (it takes `SegmentOpts`).

```rust
/// `/videos/{id}/h264cmaf/main.m3u8` — video-only h264 fMP4 media playlist.
async fn h264cmaf_variant(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    path: web::Path<String>,
) -> Result<HttpResponse, actix_web::Error> {
    let id = path.into_inner();
    let item = load_hls_item(&state, &id).await?;
    let duration = item.duration_seconds;
    let segment_count = ((duration / SEGMENT_SECONDS).ceil() as u32).max(1);
    let qs = playback_qs(&req);
    let mut body = String::with_capacity(256 + segment_count as usize * 48);
    body.push_str("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-PLAYLIST-TYPE:VOD\n");
    body.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", SEGMENT_SECONDS as u32));
    body.push_str(&format!("#EXT-X-MAP:URI=\"/videos/{id}/h264cmaf/init.mp4?{qs}\"\n"));
    let start_ticks = parse_start_time_ticks_qs(req.query_string());
    if start_ticks > 0 {
        let secs = Ticks(start_ticks).seconds();
        body.push_str(&format!("#EXT-X-START:TIME-OFFSET={secs:.3},PRECISE=YES\n"));
    }
    body.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
    for seg in 0..segment_count {
        let (start_secs, dur_secs) = segment_time_range(seg, item.frame_rate_mille);
        let len = dur_secs.min((duration - start_secs).max(0.01));
        body.push_str(&format!("#EXTINF:{len:.3},\n/videos/{id}/h264cmaf/{seg}.m4s?{qs}\n"));
    }
    body.push_str("#EXT-X-ENDLIST\n");
    Ok(HttpResponse::Ok()
        .content_type("application/vnd.apple.mpegurl")
        .insert_header(playlist_cache_control(false))
        .body(body))
}

/// `/videos/{id}/h264cmaf/init.mp4` — shared fMP4 init (h264).
async fn h264cmaf_init(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    path: web::Path<String>,
    q: CiQuery<SegmentQuery>,
) -> Result<HttpResponse, actix_web::Error> {
    let id_num: u64 = pharos_jellyfin_api::dto::parse_item_id(&path.into_inner())
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    let item = fetch_item(&state, id_num).await?;
    check_session(&state, q.play_session_id.as_deref()).await?;
    let mut opts = fmp4_segment_opts(&state, &req, &item, 0, SegmentVideo::H264, q.audio_stream_index, q.subtitle_stream_index).await;
    {
        let (start_secs, dur_secs) = segment_time_range(0, item.probe.frame_rate_mille);
        gate_image_sub_burn(&state, &item, &mut opts, start_secs, dur_secs).await;
    }
    resolve_text_burn_assets(&state, &item, &mut opts).await;
    let raw = vp9_segment_raw(&state, &item, 0, &opts).await?;
    let processed = fmp4::process_segment(&raw)
        .map_err(|e| error::ErrorInternalServerError(format!("fmp4 init: {e}")))?;
    Ok(HttpResponse::Ok()
        .content_type("video/mp4")
        .insert_header((actix_web::http::header::CACHE_CONTROL, "public, max-age=31536000, immutable"))
        .body(processed.init))
}

/// `/videos/{id}/h264cmaf/{seg}.m4s` — one h264 fMP4 media segment.
async fn h264cmaf_segment(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    path: web::Path<(String, u32)>,
    q: CiQuery<SegmentQuery>,
) -> Result<HttpResponse, actix_web::Error> {
    // Copy the body of `vp9_segment` verbatim, but build opts with
    // `fmp4_segment_opts(..., SegmentVideo::H264, ...)`. Everything else
    // (backfill yield, cache, fmp4::process_segment + tfdt correction,
    // Content-Type video/mp4) is codec-agnostic and identical.
}
```

For `h264cmaf_segment`, read the full current `vp9_segment` body (`hls.rs:1788`+) and reproduce it, substituting the opts constructor. Keep the `fmp4::process_segment` tfdt correction — it operates on mp4 boxes and is codec-agnostic.

- [ ] **Step 4: Register the routes**

In `register` (`hls.rs:37`), after the vp9 routes (~85), add:

```rust
.route("/videos/{id}/h264cmaf/main.m3u8", web::get().to(h264cmaf_variant))
.route("/videos/{id}/h264cmaf/init.mp4", web::get().to(h264cmaf_init))
.route("/videos/{id}/h264cmaf/{seg}.m4s", web::get().to(h264cmaf_segment))
```

- [ ] **Step 5: Run the playlist test + a segment fetch smoke test**

Run: `nix develop --command cargo test -p pharos-server --lib h264cmaf 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/pharos-server/src/api/jellyfin/hls.rs
git commit -m "feat(hls): h264-CMAF video-only fMP4 surface (playlist/init/segment)"
```

---

### Task 3: Browser-gated demuxed-h264 master rung

A *browser* client (Mozilla UA) whose master codec is h264 gets a video-only h264-fMP4 rung + the demuxed audio group. Native / no-Mozilla keeps the muxed mpegts rungs.

**Files:**
- Modify: `crates/pharos-server/src/api/jellyfin/hls.rs` (`master_playlist` ~637-721; add `push_h264_cmaf_rungs` + `is_web_client`)

**Interfaces:**
- Consumes: `select_master_video` (existing), `codecs_string` (~308), the `/vp9/audio.m3u8` audio rendition.
- Produces: `is_web_client(req) -> bool`; `push_h264_cmaf_rungs(body, id, qs, item)`.

- [ ] **Step 1: Write failing tests — web→demuxed, native→muxed-unchanged**

```rust
#[actix_web::test]
async fn browser_h264_master_is_demuxed_cmaf() {
    let (app, token) = hls_test_app().await;
    let req = test::TestRequest::get()
        .uri(&format!("/videos/9/master.m3u8?api_key={token}&renditions=h264"))
        .insert_header((actix_web::http::header::USER_AGENT, "Mozilla/5.0 Firefox/152.0"))
        .to_request();
    let body = String::from_utf8(test::call_and_read_body(&app, req).await.to_vec()).unwrap();
    assert!(body.contains("#EXT-X-MEDIA:TYPE=AUDIO"), "demuxed audio group present");
    assert!(body.contains("/videos/9/h264cmaf/main.m3u8"), "video-only h264-cmaf rung");
    assert!(body.contains("AUDIO=\"aud\""), "video rung binds the audio group");
    assert!(!body.contains("/videos/9/hls1/") && !body.contains("/Videos/9/main.m3u8"),
        "no muxed mpegts rung in a demuxed master");
    assert!(body.contains("avc1") && body.contains("opus"), "CODECS list video+group audio");
}

#[actix_web::test]
async fn native_h264_master_stays_muxed_mpegts() {
    let (app, token) = hls_test_app().await;
    // No Mozilla UA, no renditions → native single-rung muxed path (unchanged).
    let req = test::TestRequest::get()
        .uri(&format!("/videos/9/master.m3u8?api_key={token}"))
        .insert_header((actix_web::http::header::USER_AGENT, "AndroidTV/1.0"))
        .to_request();
    let body = String::from_utf8(test::call_and_read_body(&app, req).await.to_vec()).unwrap();
    assert!(body.contains("/Videos/9/main.m3u8"), "muxed mpegts main rung");
    assert!(!body.contains("h264cmaf"), "native must not get the CMAF rung");
    assert!(!body.contains("#EXT-X-MEDIA:TYPE=AUDIO"), "native master has no demuxed group");
}
```

- [ ] **Step 2: Run — expect the browser test to fail (still muxed)**

Run: `nix develop --command cargo test -p pharos-server --lib browser_h264_master_is_demuxed_cmaf native_h264_master_stays_muxed_mpegts 2>&1 | tail`
Expected: `browser_...` FAILS (no CMAF rung yet); `native_...` passes.

- [ ] **Step 3: Add `is_web_client` + `push_h264_cmaf_rungs`**

```rust
/// A browser (hls.js) client — the only surface that gets the demuxed
/// all-fMP4 master. Mirrors the `is_web_client` UA test used when building
/// `renditions` in PlaybackInfo (a Mozilla token). Native players (Android
/// TV, etc.) fail this and keep the muxed mpegts path.
fn is_web_client(req: &HttpRequest) -> bool {
    req.headers()
        .get(actix_web::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ua| ua.contains("Mozilla"))
}

/// Video-only h264 fMP4 rung + the shared demuxed audio group (all-fMP4).
fn push_h264_cmaf_rungs(body: &mut String, id: &str, qs: &str, item: &HlsItem) {
    let bitrate = effective_video_bitrate(None, item.source_bitrate_bps) + 128_000;
    // Video codec (re-encoded h264) + the group's Opus audio.
    let codecs = codecs_string(Some("h264"), None, None, Some("opus"));
    body.push_str(&format!(
        "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Audio\",DEFAULT=YES,\
         AUTOSELECT=YES,URI=\"/videos/{id}/vp9/audio.m3u8?{qs}\"\n"
    ));
    let resolution = match (item.width, item.height) {
        (Some(w), Some(h)) => format!(",RESOLUTION={w}x{h}"),
        _ => String::new(),
    };
    body.push_str(&format!(
        "#EXT-X-STREAM-INF:BANDWIDTH={bitrate},CODECS=\"{codecs}\",AUDIO=\"aud\"{resolution}\n\
         /videos/{id}/h264cmaf/main.m3u8?{qs}\n"
    ));
}
```

- [ ] **Step 4: Branch the master on web+h264**

In `master_playlist`, the `match video { … }` block (~713). The h264 arm becomes:

```rust
match video {
    MasterVideo::H264 => {
        if is_web_client(&req) {
            // Browser: demuxed all-fMP4 (seamless-ish audio switch, cache-hit video).
            // fMP4 media requires HLS v7 — but the header was written above as v3.
            // Move the version decision to account for a demuxed-h264 master too.
            push_h264_cmaf_rungs(&mut body, &id, &qs, &item);
        } else {
            push_h264_rungs(&mut body, &id, &qs, &item, bitrate_cap);
        }
    }
    MasterVideo::Vp9 => push_vp9_rungs(&mut body, &id, &qs, &item),
}
```

**Important — HLS version:** the `has_fmp4` computation (~663) that picks `#EXT-X-VERSION:7` currently only fires for `MasterVideo::Vp9`. A demuxed-h264 master ALSO serves fMP4, so it needs v7. Update:

```rust
let demuxed_h264 = !is_audio && video == MasterVideo::H264 && is_web_client(&req);
let has_fmp4 = !is_audio && (video == MasterVideo::Vp9 || demuxed_h264);
```

- [ ] **Step 5: Run both master tests + the full hls.rs unit suite**

Run: `nix develop --command cargo test -p pharos-server --lib master 2>&1 | tail -25`
Expected: PASS — browser gets CMAF, native unchanged, and the existing master tests (`master_playlist_uses_real_resolution_and_bitrate_from_probe`, `vp9_first_request_collapses_to_a_clean_vp9_master`, `master_playlist_lists_each_variant_below_source_height`) still pass. If a pre-existing master test asserts muxed output WITHOUT a UA header, it exercises the native path (no Mozilla) → still muxed → stays green.

- [ ] **Step 6: Commit**

```bash
git add crates/pharos-server/src/api/jellyfin/hls.rs
git commit -m "feat(hls): route browser h264 masters to a demuxed-audio CMAF rung"
```

---

### Task 4: Cache-key audio-independence guard test

Lock the core invariant: two h264-cmaf video-segment requests differing only in `AudioStreamIndex` resolve to the SAME cache entry (else the whole feature is moot).

**Files:**
- Modify: `crates/pharos-server/tests/jellyfin_hls_multivariant.rs`

**Interfaces:**
- Consumes: the h264cmaf routes (Task 2), the master gate (Task 3).

- [ ] **Step 1: Write the failing/guard test**

Spin pharos on an ephemeral port (reuse the harness already in this test file — copy its server-boot + auth helper). Fetch `/videos/{id}/h264cmaf/0.m4s?...&AudioStreamIndex=1` then `...&AudioStreamIndex=2`; assert BOTH succeed and the second is served fast (cache hit) — assert byte-identical bodies (audio-free video is independent of the audio track).

```rust
#[tokio::test]
async fn h264cmaf_video_segment_is_identical_across_audio_indices() {
    let srv = boot_pharos_with_media().await; // reuse this file's harness
    let s1 = srv.get(&format!("/videos/{id}/h264cmaf/0.m4s?api_key={t}&PlaySessionId=p&AudioStreamIndex=1")).await;
    let s2 = srv.get(&format!("/videos/{id}/h264cmaf/0.m4s?api_key={t}&PlaySessionId=p&AudioStreamIndex=2")).await;
    assert_eq!(s1.status(), 200);
    assert_eq!(s2.status(), 200);
    assert_eq!(s1.bytes, s2.bytes, "audio-free video segment must not vary by audio track");
}
```

If this test file has no such harness, add the test to `client_compat.rs` instead (which already boots a real server) and adapt to its client shape.

- [ ] **Step 2: Run it**

Run: `nix develop --command cargo nextest run -p pharos-server -E 'test(h264cmaf_video_segment_is_identical_across_audio_indices)' 2>&1 | tail`
Expected: PASS (the segments are audio-free → identical).

- [ ] **Step 3: Commit**

```bash
git add crates/pharos-server/tests/jellyfin_hls_multivariant.rs
git commit -m "test(hls): h264-cmaf video segment is audio-track-independent"
```

---

### Task 5: Full verification + doctests + spawn backend

**Files:** none (verification only).

- [ ] **Step 1: Clippy (both backends)**

Run: `nix develop --command cargo clippy -p pharos-server --all-targets 2>&1 | grep -E 'warning|error'`
Then: `nix develop --command cargo clippy -p pharos-server --no-default-features --features ffmpeg-spawn --all-targets 2>&1 | grep -E 'warning|error'`
Expected: no output (clean) for both.

- [ ] **Step 2: Spawn backend build**

Run: `nix develop --command cargo build -p pharos-server --no-default-features --features ffmpeg-spawn 2>&1 | tail -3`
Expected: `Finished`.

- [ ] **Step 3: Full workspace test**

Run: `nix develop --command just test 2>&1 | tail -6`
Expected: all pass (VP9 `vp9_fmp4_hls.rs`, muxed-h264, client_compat all green — muxed + vp9 untouched).

- [ ] **Step 4: Doctests**

Run: `nix develop --command cargo test --doc -p pharos-server 2>&1 | tail -4`
Expected: pass.

- [ ] **Step 5: Final commit if any verification-driven fixups were needed**

(Only if steps 1–4 surfaced fixes — otherwise nothing to commit.)

---

## Self-Review Notes

- **Spec coverage:** Task 1 (parameterize) + Task 2 (h264-cmaf surface) + Task 3 (browser-gated master) implement the spec's three components; Task 4 locks the audio-independence invariant; Task 5 is the compat/regression gate. Audio rendition reuse = no new audio code (spec §Approach.2). Native untouched (spec §Client compatibility) = Task 3 native test.
- **HLS v7 gotcha:** a demuxed-h264 master serves fMP4 → must advertise `#EXT-X-VERSION:7` (Task 3 Step 4). Missing this is a silent Safari/hls.js parse failure.
- **CODECS string:** demuxed rung lists `avc1…,opus` (video + group audio), via `codecs_string(Some("h264"),None,None,Some("opus"))` — NOT `hls_output_codecs_string` (that appends `aac` for the muxed path).
- **fMP4 tfdt correction:** `fmp4::process_segment` is codec-agnostic (operates on mp4 boxes); reused unchanged for h264 (Task 2).
- **Audio endpoint reuse:** the h264-cmaf master points at `/vp9/audio.m3u8`. Semantically odd name, functionally correct (codec-neutral Opus). Renaming is out of scope (would touch the vp9 path).
