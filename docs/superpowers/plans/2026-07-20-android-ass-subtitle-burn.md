# Android ASS Subtitle Burn (honor SubtitleProfiles) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop the Jellyfin Android app from rendering ASS subtitles as black bars by having pharos honor the client's declared `SubtitleProfiles` and BURN ASS into the (already-running) transcode for clients that can't render it externally — while jellyfin-web keeps getting External ASS (SubtitlesOctopus).

**Architecture:** pharos currently decides subtitle delivery purely by `is_text` (text→External, image→Encode) in `build_media_streams_with_subtitles` (dto.rs:2260), ignoring the client's `DeviceProfile.SubtitleProfiles`. We add SubtitleProfiles parsing, a pure per-format delivery decision (`External` vs `Burn`), thread the decision through the PlaybackInfo MediaStream DTOs and the transcode-URL subtitle-index forwarding, downgrade a DirectPlay decision to Transcode when the selected subtitle must burn (so there is always a video stream to burn into — mirrors the B80/V40 Matroska downgrade), and extend the segment burn path (currently `overlay`, image-only) to rasterize ASS/text via the ffmpeg `subtitles=` filter with per-segment timestamp alignment. When the client is already transcoding, the burn is just an added filter; when it would direct-play, the downgrade makes the transcode exist.

**Tech Stack:** Rust, actix-web, serde (PascalCase Jellyfin DTOs), ffmpeg (libav in-process worker + spawn), nextest. Devshell: all `cargo`/`nextest`/`ffmpeg` via `nix develop --command …`.

## Global Constraints

- All builds/tests run inside the Nix devShell: `nix develop --command <cmd>`.
- No `unwrap()`/`expect()` in non-test code (V17, `clippy::unwrap_used`/`expect_used` = deny). Tests may opt out with `#![allow(clippy::unwrap_used, clippy::expect_used)]`.
- Run `just test` (full workspace) green before any commit; `cargo clippy --all-targets` must be clean (pre-commit only runs rustfmt — clippy is a separate CI gate).
- Any ffmpeg flag doing timeline/mux/render work needs a BEHAVIORAL test that runs ffmpeg and inspects the OUTPUT (V30 corollary) — an argv-string assertion passes even when the muxer/filter ignores the flag.
- Atomic commits, one logical change each; never squash. Bug/feature ties to §B/§V go through the spec skill.
- Wire DTOs that a strict SDK deserializes stay typed `#[derive(Serialize)]` structs, PascalCase (V38); acronym renames explicit.
- Times ISO8601 UTC.

---

## Background facts (verified 2026-07-20, do not re-derive)

- Repro: NGE S01E15 (media.id 9067691139041605900, wire id `7dd6e90c5b88390c`), Jellyfin Android app (UA `Ktor client`). PlaybackInfo shows subtitle tracks 4/5/6 = `ass` delivered `External`, tracks 7–10 = `hdmv_pgs_subtitle` delivered `Encode`. The session is TRANSCODING (`SupportsDirectPlay:false`, `TranscodingUrl` = HLS `master.m3u8`).
- pharos serves a valid 315 KB ASS with heavy typesetting + custom embedded fonts. The black bars are the Android player's own weak ASS renderer, not malformed data.
- `SubtitleProfile` appears NOWHERE in the codebase — the client's declared profiles are never parsed.
- Burn today: `push_video_filters` (crates/pharos-transcode/src/lib.rs:210) emits `[0:v:0][0:s:{si}]overlay=eof_action=pass[vout]`. `overlay` composites a BITMAP sub stream — correct for PGS/VOBSUB, wrong for text/ASS (text must be rasterized by the `subtitles=`/`ass=` filter).
- Burn is currently restricted to image codecs at three gates: (a) items.rs PlaybackInfo transcode-URL forwarding (~2153) only forwards `SubtitleStreamIndex` when the track is NOT text; (b) h264 `build_segment_opts` and (c) VP9 `vp9_segment_opts` (hls.rs ~2036) compute `sub_rel` only when `is_image_subtitle_codec`.
- pharos already pre-extracts + disk-caches embedded ASS (`SubtitleКind::EmbeddedAss`) at scan time (memory: subtitle_extraction_cost).

---

## Task 1: Parse + observe client SubtitleProfiles

Grounds the whole feature: confirm what the Android app actually declares before building decision logic on an assumption.

**Files:**
- Modify: `crates/pharos-jellyfin-api/src/device_profile.rs:22-34` (add field + struct)
- Modify: `crates/pharos-server/src/api/jellyfin/items.rs` (temporary INFO log of parsed subtitle_profiles in the PlaybackInfo handler, near the DeviceProfile consume at ~1887-1896)
- Test: `crates/pharos-jellyfin-api/src/device_profile.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Produces: `SubtitleProfileDto { format: String, method: String, protocol: String, language: String }` (all `#[serde(default)]`, PascalCase `Format`/`Method`/`Protocol`/`Language`); `DeviceProfile.subtitle_profiles: Vec<SubtitleProfileDto>`.

- [ ] **Step 1: Write the failing test**

```rust
// in device_profile.rs #[cfg(test)] mod tests
#[test]
fn parses_subtitle_profiles_from_pascalcase() {
    let json = r#"{"SubtitleProfiles":[
        {"Format":"ass","Method":"Encode"},
        {"Format":"subrip","Method":"External"}]}"#;
    let p: super::DeviceProfile = serde_json::from_str(json).unwrap();
    assert_eq!(p.subtitle_profiles.len(), 2);
    assert_eq!(p.subtitle_profiles[0].format, "ass");
    assert_eq!(p.subtitle_profiles[0].method, "Encode");
    assert_eq!(p.subtitle_profiles[1].method, "External");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `nix develop --command cargo test -p pharos-jellyfin-api parses_subtitle_profiles_from_pascalcase`
Expected: FAIL — no field `subtitle_profiles` / no `SubtitleProfileDto`.

- [ ] **Step 3: Add the struct + field**

```rust
// device_profile.rs — new struct near CodecProfileDto
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct SubtitleProfileDto {
    #[serde(default)]
    pub format: String,
    /// Jellyfin: "Encode" (burn) | "Embed" | "External" | "Hls".
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub protocol: String,
    #[serde(default)]
    pub language: String,
}
```

```rust
// device_profile.rs — add to `struct DeviceProfile` (after codec_profiles)
    #[serde(default)]
    pub subtitle_profiles: Vec<SubtitleProfileDto>,
```

- [ ] **Step 4: Run test to verify it passes**

Run: `nix develop --command cargo test -p pharos-jellyfin-api parses_subtitle_profiles_from_pascalcase`
Expected: PASS.

- [ ] **Step 5: Add a temporary observability log**

In items.rs PlaybackInfo handler, right after the DeviceProfile is parsed (the `b.device_profile.unwrap_or_default()` site ~1892), add:

```rust
tracing::info!(
    client.ua = %req_user_agent, // whatever the handler already has; else derive from req headers
    subtitle_profiles = ?device_profile.subtitle_profiles,
    "playbackinfo: client subtitle profiles"
);
```

(If a UA var isn't in scope, log just `subtitle_profiles`.) This is temporary — removed in Task 8.

- [ ] **Step 6: Commit**

```bash
git add crates/pharos-jellyfin-api/src/device_profile.rs crates/pharos-server/src/api/jellyfin/items.rs
git commit -m "feat(profile): parse client DeviceProfile.SubtitleProfiles (+ temp observability log)"
```

- [ ] **Step 7: Deploy + capture the Android app's real declaration**

Push (CI → GHCR → Flux rollout, ~15 min), then on the Android app open NGE S01E15 and read:
`kubectl logs -n pharos <pod> | rg "client subtitle profiles" | rg -i ktor | tail -1`
Record the exact `Method` the app declares for `ass`/`ssa`. **This confirms the Task 2 rule.** If the app declares `ass`→`External` (media3 over-promising), note it: the decision rule must then treat the Android app as burn-required by UA class, not by declared method — adjust Task 2's rule accordingly before proceeding.

---

## Task 2: Pure subtitle-delivery decision

**Files:**
- Create: `crates/pharos-server/src/api/jellyfin/subtitle_delivery.rs`
- Modify: `crates/pharos-server/src/api/jellyfin/mod.rs` (add `mod subtitle_delivery;`)
- Test: same file, inline `#[cfg(test)]`

**Interfaces:**
- Consumes: `pharos_jellyfin_api::device_profile::SubtitleProfileDto`; `is_text_subtitle_codec` / `is_image_subtitle_codec` (crate::api::jellyfin::dto / subtitles).
- Produces:
  ```rust
  pub enum SubtitleDelivery { External, Burn }
  pub fn decide_subtitle_delivery(
      codec: Option<&str>,
      client_profiles: &[SubtitleProfileDto],
  ) -> SubtitleDelivery
  ```

**Rule (adjust per Task 1 Step 7 evidence):**
- Image codec (PGS/VOBSUB) → `Burn` (unchanged behavior).
- Text codec: find a client SubtitleProfile whose `format` matches the codec (ass↔"ass"/"ssa"/"subrip" etc., case-insensitive) with an external-capable `method` (`External`/`Embed`/`Hls`) → `External`. Otherwise (`method=Encode`, or the format is absent from the client's profiles, or `client_profiles` is empty) → `Burn`.
- Empty `client_profiles` → `Burn` is WRONG for the /Items path (no client context) — but this fn is only called on the PlaybackInfo path where profiles are present. The `None`/empty case for text defaults to `External` to preserve the current default for profile-less callers. (Encode requires a client that asked for it.)

> Decision note: choose ONE default for empty-profiles-text and encode it in a test. Given jellyfin-web always sends ass→External and the Android app is the one needing burn, defaulting empty→External is safe (only a real Encode-declaring or ass-absent profile triggers burn).

- [ ] **Step 1: Write failing tests**

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use pharos_jellyfin_api::device_profile::SubtitleProfileDto;

fn prof(fmt: &str, method: &str) -> SubtitleProfileDto {
    SubtitleProfileDto { format: fmt.into(), method: method.into(), ..Default::default() }
}

#[test]
fn web_declares_ass_external_gets_external() {
    let p = [prof("ass", "External"), prof("subrip", "External")];
    assert!(matches!(decide_subtitle_delivery(Some("ass"), &p), SubtitleDelivery::External));
}
#[test]
fn client_declaring_ass_encode_gets_burn() {
    let p = [prof("ass", "Encode"), prof("subrip", "External")];
    assert!(matches!(decide_subtitle_delivery(Some("ass"), &p), SubtitleDelivery::Burn));
}
#[test]
fn client_without_ass_profile_gets_burn() {
    let p = [prof("subrip", "External"), prof("vtt", "External")];
    assert!(matches!(decide_subtitle_delivery(Some("ass"), &p), SubtitleDelivery::Burn));
}
#[test]
fn image_codec_always_burns() {
    let p = [prof("ass", "External")];
    assert!(matches!(decide_subtitle_delivery(Some("hdmv_pgs_subtitle"), &p), SubtitleDelivery::Burn));
}
#[test]
fn empty_profiles_text_defaults_external() {
    assert!(matches!(decide_subtitle_delivery(Some("ass"), &[]), SubtitleDelivery::External));
}
```

- [ ] **Step 2: Run to verify fail**

Run: `nix develop --command cargo test -p pharos-server subtitle_delivery`
Expected: FAIL — module/fn absent.

- [ ] **Step 3: Implement**

```rust
//! Per-format subtitle delivery decision from the client's DeviceProfile.
use pharos_jellyfin_api::device_profile::SubtitleProfileDto;
use crate::api::jellyfin::subtitles::is_image_subtitle_codec;
use crate::api::jellyfin::dto::is_text_subtitle_codec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleDelivery { External, Burn }

fn format_matches(codec: &str, fmt: &str) -> bool {
    let c = codec.to_ascii_lowercase();
    let f = fmt.to_ascii_lowercase();
    c == f
        || (matches!(c.as_str(), "ass" | "ssa" | "advanced substation alpha")
            && matches!(f.as_str(), "ass" | "ssa"))
        || (c == "subrip" && matches!(f.as_str(), "subrip" | "srt"))
}

fn method_is_external(method: &str) -> bool {
    matches!(method.to_ascii_lowercase().as_str(), "external" | "embed" | "hls")
}

pub fn decide_subtitle_delivery(
    codec: Option<&str>,
    client_profiles: &[SubtitleProfileDto],
) -> SubtitleDelivery {
    let codec = codec.unwrap_or("");
    if is_image_subtitle_codec(&codec.to_ascii_lowercase()) {
        return SubtitleDelivery::Burn;
    }
    if !is_text_subtitle_codec(Some(codec)) {
        return SubtitleDelivery::Burn; // unknown/other → safest is burn
    }
    if client_profiles.is_empty() {
        return SubtitleDelivery::External; // profile-less caller keeps the default
    }
    let has_external = client_profiles
        .iter()
        .any(|p| format_matches(codec, &p.format) && method_is_external(&p.method));
    if has_external { SubtitleDelivery::External } else { SubtitleDelivery::Burn }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `nix develop --command cargo test -p pharos-server subtitle_delivery`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/pharos-server/src/api/jellyfin/subtitle_delivery.rs crates/pharos-server/src/api/jellyfin/mod.rs
git commit -m "feat(subs): pure per-format subtitle delivery decision from client SubtitleProfiles"
```

---

## Task 3: Thread the decision into the PlaybackInfo MediaStream DTOs

**Files:**
- Modify: `crates/pharos-jellyfin-api/src/dto.rs:2042-2262` (`SubtitleStreamCtx` + the text-sub branch)
- Modify: `crates/pharos-server/src/api/jellyfin/items.rs` (PlaybackInfo builds the ctx with the client's resolved per-track delivery)
- Test: `crates/pharos-jellyfin-api/src/dto.rs` inline tests (near existing 2687/2710 delivery_method asserts)

**Interfaces:**
- Consumes: `SubtitleDelivery` (Task 2). To keep pharos-jellyfin-api free of a server-crate dependency, resolve the decision in items.rs (server crate) and pass a resolved per-stream-index set of "burn these" into the ctx.
- Produces: `SubtitleStreamCtx.burn_text_indices: std::collections::BTreeSet<u32>` — absolute ffprobe indices of TEXT subs the client will burn (so the DTO emits `delivery_method: "Encode"`, `supports_external_stream: false`, no `delivery_url`).

- [ ] **Step 1: Write failing test**

```rust
// dto.rs tests — a ctx that marks an ass track (index 4) as burn
#[test]
fn ass_track_marked_burn_emits_encode_not_external() {
    let probe = /* MediaProbe with one ass subtitle_track at stream_index 4 */;
    let mut ctx = SubtitleStreamCtx::new(MediaId(1));
    ctx.burn_text_indices.insert(4);
    let streams = build_media_streams_with_subtitles(&probe, true, Some(&ctx));
    let ass = streams.iter().find(|s| s.index == 4).unwrap();
    assert_eq!(ass.delivery_method, Some("Encode"));
    assert_eq!(ass.supports_external_stream, false);
    assert!(ass.delivery_url.is_none());
    assert_eq!(ass.is_text_subtitle_stream, true); // still text; just burned
}
#[test]
fn ass_track_not_marked_stays_external() {
    let probe = /* same one-ass-track probe */;
    let ctx = SubtitleStreamCtx::new(MediaId(1)); // empty burn set
    let streams = build_media_streams_with_subtitles(&probe, true, Some(&ctx));
    let ass = streams.iter().find(|s| s.index == 4).unwrap();
    assert_eq!(ass.delivery_method, Some("External"));
    assert!(ass.delivery_url.is_some());
}
```

- [ ] **Step 2: Run — fail** (`burn_text_indices` field missing).
Run: `nix develop --command cargo test -p pharos-jellyfin-api ass_track_marked_burn`

- [ ] **Step 3: Implement ctx field + branch**

Add to `SubtitleStreamCtx`:
```rust
    /// Absolute ffprobe stream indices of TEXT subs the client will BURN
    /// (its SubtitleProfile lacks external support for that format). Such a
    /// track is advertised Encode/no-DeliveryUrl so the client requests the
    /// burn transcode instead of trying to render the raw ASS itself.
    pub burn_text_indices: std::collections::BTreeSet<u32>,
```
Init to `BTreeSet::new()` in `new()`.

In the embedded text-sub branch (dto.rs ~2253-2262), replace the unconditional `is_text` delivery with:
```rust
let burn = ctx.burn_text_indices.contains(&t.stream_index);
let external = is_text && !burn;
// ...
delivery_url: external.then(|| format!(
    "/Videos/{id}/{id}/Subtitles/{idx}/Stream.{ext}",
    id = wire_item_id(ctx.item_id), idx = t.stream_index,
)),
delivery_method: Some(if is_text {
    if burn { "Encode" } else { "External" }
} else { "Encode" }),
is_text_subtitle_stream: is_text,
supports_external_stream: external,
```

- [ ] **Step 4: Run — pass.** `nix develop --command cargo test -p pharos-jellyfin-api ass_track`

- [ ] **Step 5: Populate `burn_text_indices` in items.rs**

Where PlaybackInfo builds the `SubtitleStreamCtx`, for each text subtitle track call `decide_subtitle_delivery(track.codec, &device_profile.subtitle_profiles)`; insert the track's `stream_index` into `ctx.burn_text_indices` when it returns `Burn`. Add an items-level test asserting an Android-shaped profile (no external ass) yields a burn index while a web-shaped profile (ass External) yields none.

- [ ] **Step 6: Commit**

```bash
git add crates/pharos-jellyfin-api/src/dto.rs crates/pharos-server/src/api/jellyfin/items.rs
git commit -m "feat(subs): advertise Encode for text subs the client can't render externally"
```

---

## Task 4: Forward the text-sub burn index into the transcode URL

Currently items.rs (~2153) forwards `SubtitleStreamIndex` into the HLS master URL ONLY for image subs. A burned text sub must also be forwarded so the segment handler burns it.

**Files:**
- Modify: `crates/pharos-server/src/api/jellyfin/items.rs:2153-2190`
- Test: `crates/pharos-server/tests/subtitle_delivery.rs` (or extend an existing playbackinfo test)

**Interfaces:**
- Consumes: `SubtitleStreamCtx.burn_text_indices` (the same set), or re-run `decide_subtitle_delivery`. Reuse the set for a single source of truth.

- [ ] **Step 1: Write failing test** — an Android-profile PlaybackInfo for an item whose selected sub (or default) is ass ⇒ the returned `TranscodingUrl` query contains `SubtitleStreamIndex=<idx>`. A web-profile PlaybackInfo ⇒ it does NOT.

- [ ] **Step 2: Run — fail.**

- [ ] **Step 3: Factor a shared selected-subtitle resolver**

Extract the "which subtitle track applies" logic (explicit `SubtitleStreamIndex`/body pick, else the container's default-disposition track) into one helper reused by both this task and Task 5:
```rust
/// The subtitle track this playback will act on: the client's explicit pick
/// (query `SubtitleStreamIndex` or body), else the default-disposition track,
/// else None. `-1` (off) → None.
fn resolve_selected_subtitle<'a>(
    explicit_index: Option<i64>,
    probe: &'a pharos_core::MediaProbe,
) -> Option<&'a pharos_core::SubtitleTrack> { /* … */ }
```

- [ ] **Step 4: Implement the forward predicate** — forward `SubtitleStreamIndex` when `-1`, OR the selected track is an image sub, OR the selected track's index is in `burn_text_indices`. Same for the default-disposition branch.

- [ ] **Step 5: Run — pass.**

- [ ] **Step 6: Commit** `feat(subs): forward burned text-sub index into the transcode URL`.

---

## Task 5: Downgrade DirectPlay → Transcode when the selected subtitle must burn

**No gap:** a client that would DIRECT-PLAY the file but selects (or defaults to) a subtitle it can't render externally — a text/ASS sub with no external SubtitleProfile support, OR any image sub — has no video stream to burn into. Mirror the B80/V40 Matroska downgrade: force a Transcode so a `TranscodingUrl` is emitted (carrying the burn index from Task 4) and `SupportsDirectStream` resolves false. This also fixes today's silent bug where a DirectPlay client selecting a PGS/default-image sub simply showed no subtitle.

**Files:**
- Modify: `crates/pharos-server/src/api/jellyfin/items.rs` (new predicate + a second downgrade right after the `browser_matroska_direct_unplayable` one at ~2000-2007, BEFORE `force_webm`/`direct_play`/`supports_direct_stream` compute at ~2089)
- Test: `crates/pharos-server` unit tests for the predicate + an items PlaybackInfo test

**Interfaces:**
- Consumes: `resolve_selected_subtitle` (Task 4), `decide_subtitle_delivery` (Task 2), `Decision` (`is_direct()`), `profile.subtitle_profiles`.
- Produces:
  ```rust
  /// A DirectPlay/DirectStream decision must downgrade to Transcode when the
  /// selected subtitle will be BURNED (no external stream to render it).
  fn subtitle_selection_forces_transcode(
      decision: &Decision,
      selected: Option<&pharos_core::SubtitleTrack>,
      client_profiles: &[SubtitleProfileDto],
  ) -> bool
  ```

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn directplay_with_burned_ass_forces_transcode() {
    let sel = /* ass SubtitleTrack idx 4 */;
    let profiles = [/* subrip External only — no external ass */];
    assert!(subtitle_selection_forces_transcode(&Decision::DirectPlay, Some(&sel), &profiles));
}
#[test]
fn directplay_with_externally_rendered_ass_does_not_transcode() {
    let sel = /* ass idx 4 */;
    let profiles = [/* ass External */];
    assert!(!subtitle_selection_forces_transcode(&Decision::DirectPlay, Some(&sel), &profiles));
}
#[test]
fn directplay_with_image_sub_forces_transcode() {
    let sel = /* hdmv_pgs_subtitle idx 7 */;
    assert!(subtitle_selection_forces_transcode(&Decision::DirectPlay, Some(&sel), &[]));
}
#[test]
fn no_subtitle_selected_keeps_directplay() {
    assert!(!subtitle_selection_forces_transcode(&Decision::DirectPlay, None, &[]));
}
#[test]
fn already_transcoding_is_noop() {
    let sel = /* ass idx 4 */;
    assert!(!subtitle_selection_forces_transcode(
        &Decision::Transcode { /* … */ }, Some(&sel), &[]));
}
```

- [ ] **Step 2: Run — fail.** `nix develop --command cargo test -p pharos-server subtitle_selection_forces_transcode`

- [ ] **Step 3: Implement the predicate**

```rust
fn subtitle_selection_forces_transcode(
    decision: &Decision,
    selected: Option<&pharos_core::SubtitleTrack>,
    client_profiles: &[SubtitleProfileDto],
) -> bool {
    if !decision.is_direct() { return false; }
    match selected {
        Some(t) => matches!(
            decide_subtitle_delivery(t.codec.as_deref(), client_profiles),
            SubtitleDelivery::Burn
        ),
        None => false,
    }
}
```

- [ ] **Step 4: Wire the downgrade into `playback_info`**

Right after the existing `browser_matroska_direct_unplayable` downgrade (items.rs ~2000-2007), add:
```rust
let selected_sub = resolve_selected_subtitle(explicit_subtitle_index, probe);
let decision = if subtitle_selection_forces_transcode(&decision, selected_sub, &profile.subtitle_profiles) {
    // V40-style: force an HLS h264/aac transcode carrying the connection ceiling,
    // so the burn has a video stream and a TranscodingUrl is emitted.
    Decision::Transcode { /* same shape the matroska downgrade builds (remote ceiling folded in) */ }
} else { decision };
```
Ensure this runs BEFORE `force_webm`/`direct_play`/`supports_direct_stream` (~2089) so those observe the downgraded decision.

- [ ] **Step 5: Run — pass** (predicate tests + an items test: Android-profile DirectPlay-eligible source with an ass default now returns a `TranscodingUrl` + `SupportsDirectStream:false`; web-profile keeps DirectPlay).

- [ ] **Step 6: Commit** `feat(subs): downgrade DirectPlay to transcode when the selected subtitle must burn (no-gap)`.

---

## Task 6: Allow text-sub burn in the segment opts

`build_segment_opts` (h264) and `vp9_segment_opts` (hls.rs ~2036) compute the burn `sub_rel` only for image codecs. Extend both to accept a text sub when the client requested burn. `gate_image_sub_burn` uses image event windows — for text subs, fail-open (always burn; the ASS filter itself no-ops on empty segments) or reuse the text event-window scan if cheap.

**Files:**
- Modify: `crates/pharos-server/src/api/jellyfin/hls.rs` (`vp9_segment_opts` ~2036, `build_segment_opts`, `gate_image_sub_burn` ~1248)
- Test: `crates/pharos-server` unit tests for the opts builders

**Interfaces:**
- Produces: `SegmentOpts.burn_subtitle_stream_index` set (codec-relative) for a text sub when the URL carries its index. A new flag distinguishes text-burn from image-burn so the transcoder picks the right filter (Task 7): extend `pharos_transcode::SegmentOpts`/`TranscodeOptions` with `burn_subtitle_is_text: bool` (default false).

- [ ] **Step 1: Write failing test** — `vp9_segment_opts` with a URL `SubtitleStreamIndex=<ass idx>` yields `burn_subtitle_stream_index = Some(rel)` and `burn_subtitle_is_text = true`; an image idx yields `is_text = false`; no idx yields `None`.

- [ ] **Step 2: Run — fail.**

- [ ] **Step 3: Implement** — drop the `is_image`-only guard on `sub_rel`; compute codec-relative index for the selected track regardless of image/text, and set `burn_subtitle_is_text` from `is_text_subtitle_codec`. In `gate_image_sub_burn`, skip the image-window gate for text subs (fail-open).

- [ ] **Step 4: Run — pass.**

- [ ] **Step 5: Commit** `feat(subs): carry text-sub burn through segment opts`.

---

## Task 7: Rasterize ASS/text in the burn filter (the hard one — includes an empirical spike)

`push_video_filters` uses `overlay` (bitmap). Text/ASS must use the `subtitles=` filter, which rasterizes the ASS and auto-loads the source's embedded attachment fonts. Per-segment input-seek requires timestamp alignment so the correct dialogue line renders for a mid-file segment.

**Files:**
- Modify: `crates/pharos-transcode/src/lib.rs:210-241` (`push_video_filters` + callers passing `burn_subtitle_is_text` + input path + seek)
- Modify: `crates/pharos-transcode/src/options.rs` / `segment.rs` (new `burn_subtitle_is_text` field, threaded)
- Test: `crates/pharos-transcode/tests/subtitle_burn_ass.rs` (NEW, ffmpeg-gated behavioral test)

**Interfaces:**
- Consumes: `TranscodeOptions { burn_subtitle_stream_index, burn_subtitle_is_text, start_position_ticks, .. }` + input path.
- Produces: for text burn, a `-filter_complex` using `subtitles=filename='<input>':si=<rel>` (escaped) instead of `overlay`.

**Approach (verify empirically in Step 1–2 — do not assume the exact flags):**
- Filter: `subtitles=filename='<input-escaped>':si=<rel>` — the `subtitles` filter reads the ass stream from the SAME source file and loads its embedded fonts. (Fallback if `si=` on a container is unreliable: burn from the pre-extracted `EmbeddedAss` sidecar via `subtitles=filename='<sidecar.ass>'`.)
- Timestamp alignment for a mid-file segment: the segment path input-seeks (`-ss START` before `-i`). The `subtitles` filter renders by frame PTS. Establish which of these yields the correct line at absolute time T inside a segment starting at START: (a) plain input-seek (filter may render from 0), (b) `-copyts` + input-seek, (c) `subtitles=...:...` with an explicit `setpts`/`-ss` after `-i`. The spike (Step 1) determines the winner empirically.

- [ ] **Step 1: Write the BEHAVIORAL test (spike)** — generate a 60 s clip whose ASS shows the literal text `"MARK@30"` only during 29–31 s (reuse the lavfi pattern from `tests/vp9_audio_rendition.rs::make_clip`; write a tiny ASS). Transcode the SEGMENT covering 30 s (start≈24–30 s, input-seek) with text burn on. Extract the middle frame (`ffmpeg -i seg -vf 'select=eq(n\,K)' -frames:v 1 f.png`) and assert the burned pixels differ from a no-burn render of the same segment (a burned region is non-empty), AND that a segment covering 10 s (no sub) is byte-identical burn-vs-no-burn in the sub region. This proves the RIGHT line renders at the RIGHT segment (not sub-from-0).

```
Run: nix develop --command cargo test -p pharos-transcode --test subtitle_burn_ass -- --ignored
Expected: FAIL first (overlay path can't rasterize ass → ffmpeg error or empty burn).
```

- [ ] **Step 2: Implement the text branch + iterate flags until Step 1 passes**

```rust
// push_video_filters — add is_text + input params
match (burn_subtitle_stream_index, burn_is_text) {
    (Some(si), true) => {
        // Rasterize ASS/text; embedded fonts load from <input>.
        let esc = ffmpeg_filter_escape(input); // escape ':' '\' '\'' for filtergraph
        let mut graph = format!("subtitles=filename='{esc}':si={si}");
        // + whatever copyts/setpts the spike proved necessary
        if !vf_parts.is_empty() { graph.push(','); graph.push_str(&vf_parts.join(",")); }
        graph.push_str("[vout]");
        a.push("-filter_complex".into()); a.push(graph);
        a.push("-map".into()); a.push("[vout]".into());
        a.push("-map".into());
        a.push(match audio_source_stream_index { Some(i)=>format!("0:a:{i}"), None=>"0:a:0?".into() });
    }
    (Some(si), false) => { /* existing overlay path, unchanged */ }
    (None, _) => { /* existing no-burn path */ }
}
```

Add `ffmpeg_filter_escape` (escape `\ ' : [ ] ,` per ffmpeg filtergraph rules) + a unit test for it.

- [ ] **Step 3: Run the behavioral test — pass.**

- [ ] **Step 4: Keep the existing image-overlay argv test green** (`burn_subtitle_uses_overlay_filter_complex`) — image path must be byte-identical.
Run: `nix develop --command cargo test -p pharos-transcode burn_subtitle`

- [ ] **Step 5: Commit** `feat(transcode): rasterize ASS/text subs via the subtitles filter for burn-in`.

---

## Task 8: Regression, cleanup, deploy, verify

**Files:**
- Modify: `crates/pharos-server/src/api/jellyfin/items.rs` (remove the Task 1 temporary log)
- Modify: `SPEC.md` (via spec skill — §B row + §V invariant)

- [ ] **Step 1: jellyfin-web regression guard** — a test asserting a web-shaped profile (ass→External) still yields `delivery_method:"External"` + a `Stream.ass` DeliveryUrl and NO `SubtitleStreamIndex` in the TranscodingUrl (octopus path untouched). Should already pass from Task 3/4 tests; add an explicit end-to-end one if missing.

- [ ] **Step 2: Remove the temporary observability log** from Task 1.

- [ ] **Step 3: Full suite + clippy**

```
nix develop --command just test
nix develop --command cargo clippy --all-targets
```
Expected: all green, clippy clean.

- [ ] **Step 4: Backprop the spec** (spec skill): §B row (Android app renders ASS as black bars; pharos ignored client SubtitleProfiles and always delivered External) + §V invariant: "subtitle DeliveryMethod is decided from the client's declared SubtitleProfiles; a text/ASS format the client cannot render externally is burned into the transcode (never advertised External), and a DirectPlay/DirectStream decision is downgraded to Transcode whenever the selected (or default-disposition) subtitle must burn — so a burn always has a video stream, extending B80/V40 to the subtitle-selection path." Cite the guard tests (decide_subtitle_delivery, subtitle_selection_forces_transcode, the ASS-burn behavioral test). Next free bug number = B104 (§B currently contiguous to B103).

- [ ] **Step 5: Commit + push (branch → ff main), deploy, verify**

On the Android app, replay NGE S01E15: subtitles now render as burned text (styled), no black bars. Confirm jellyfin-web still shows crisp octopus-rendered ASS on the same episode. Capture pod logs to confirm the ass track now returns `DeliveryMethod:"Encode"` for the Android UA and `SubtitleStreamIndex` rides the TranscodingUrl.

---

## Self-Review notes

- **Spec coverage:** honor SubtitleProfiles (T1–T3) ✓; forward burned index (T4) ✓; **DirectPlay→Transcode downgrade so a burn always has a video stream (T5)** ✓ — no gap: a client that would direct-play but selects/defaults a burn-required subtitle is downgraded, mirroring the B80/V40 Matroska-unplayable downgrade; burn in segment opts (T6) ✓; rasterize ASS (T7) ✓; keep web External + regression (T8) ✓.
- **Type consistency:** `burn_text_indices: BTreeSet<u32>` (absolute ffprobe indices) in T3; `burn_subtitle_is_text: bool` threaded T5→T6; `decide_subtitle_delivery` signature stable T2→T3→T4.
- **Empirical risk isolated to Task 7** (the ASS-burn timestamp/flags) behind a behavioral test — the spike proves correctness rather than assuming flags.
- **Open assumption (Task 1 Step 7 resolves it):** the exact `Method` the Android app declares for ass. If it declares `External` (not `Encode`), Task 2's rule must key burn on the UA/client class rather than the declared method — adjust before Task 3.
