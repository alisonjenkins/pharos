#![allow(clippy::unwrap_used, clippy::expect_used)]
//! VP9-in-fMP4 HLS end-to-end (Firefox/Zen playback path).
//!
//! The H.264/MPEG-TS ladder is useless to Firefox (no H.264 in MSE), so those
//! clients get VP9 as fMP4 HLS. This drives the real handlers through a real
//! ffmpeg transcode and asserts the wire shape hls.js needs:
//!   1. master → advertises `vp09` + points at the VP9 variant.
//!   2. variant → VOD playlist with an `EXT-X-MAP` init + `.m4s` segments.
//!   3. init.mp4 → `ftyp`+`moov`, NO `moof` (a valid shared init segment).
//!   4. `{seg}.m4s` → `moof`+`mdat`, NO `moov`/`ftyp` (moof-only media).
//!   5. segment N's `tfdt` is SOURCE-anchored (≈ N·6 s, off by at most the
//!      first-frame offset / opus preskip) and consecutive segments TILE:
//!      tfdt(N+1) ≈ tfdt(N) + content(N) per track. Forcing tfdt onto an
//!      exact 6.0 grid instead re-times video against audio by a few ms
//!      every segment (real content is ~6.012 s video / ~6.006 s audio) and
//!      accumulates into audible A/V drift over a long title. Verified
//!      through real ffmpeg output.
//!
//! `#[ignore]` + ffmpeg-gated like the other real-transcode suites; the clip
//! is generated in-test via lavfi so no fixture corpus is required.

use actix_web::{test, web, App};
use pharos_cache::HlsSegmentCache;
use pharos_core::{
    MediaItem, MediaKind, MediaProbe, MediaStore, SecretString, TokenStore, UserId, UserPolicy,
    UserRecord, UserStore,
};
use pharos_server::{api::jellyfin::hls, auth::BuiltinAuth, state::AppState};
use pharos_store_sqlx::sqlite::SqliteStore;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn ffmpeg_ok() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Generate a 15 s VP9/Opus clip so the playlist lists ≥3 segments and
/// segment 2 (start 12 s) is inside the file.
fn make_clip(dir: &Path) -> PathBuf {
    let out = dir.join("clip.webm");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=15:size=320x240:rate=24",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=15",
            "-c:v",
            "libvpx-vp9",
            "-b:v",
            "300k",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-c:a",
            "libopus",
        ])
        .arg(&out)
        .arg("-y")
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg clip generation failed");
    out
}

async fn seed(fixture: PathBuf, cache_dir: &Path) -> (web::Data<AppState>, String) {
    let stores = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let auth = BuiltinAuth::new(stores.clone());
    let hash = auth.hash_password(&SecretString::new("p")).unwrap();
    let uid = UserId::new();
    stores
        .create(UserRecord {
            id: uid,
            name: "u".into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        })
        .await
        .unwrap();
    let token = stores.issue(uid, "t").await.unwrap();
    stores
        .put(MediaItem {
            id: 42,
            path: fixture,
            title: "clip".into(),
            kind: MediaKind::Movie,
            probe: MediaProbe {
                duration_ms: Some(15_000),
                width: Some(320),
                height: Some(240),
                bitrate_bps: Some(400_000),
                video_codec: Some("vp9".into()),
                audio_codec: Some("opus".into()),
                ..Default::default()
            },
            series: None,
            created_at: None,
            metadata: Default::default(),
        })
        .await
        .unwrap();
    let cache = HlsSegmentCache::new(cache_dir, 128 * 1024 * 1024);
    let state = web::Data::new(AppState::new(stores, "t".into()).with_hls_cache(cache));
    (state, token.0.expose().to_string())
}

fn has_box(data: &[u8], fourcc: &[u8; 4]) -> bool {
    data.windows(4).any(|w| w == fourcc)
}

/// Byte-scan for every version-1 `tfdt` and return its 64-bit base decode time.
fn tfdt_values(data: &[u8]) -> Vec<u64> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 16 <= data.len() {
        if &data[i..i + 4] == b"tfdt" && data[i + 4] == 1 {
            out.push(u64::from_be_bytes(data[i + 8..i + 16].try_into().unwrap()));
        }
        i += 1;
    }
    out
}

/// Walk the direct children of `data[start..end]`: `(fourcc, body_start, box_end)`.
fn boxes(data: &[u8], start: usize, end: usize) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    let mut off = start;
    while off + 8 <= end {
        let size32 = u32::from_be_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = data[off + 4..off + 8].try_into().unwrap();
        let (hdr, size) = match size32 {
            1 => (
                16,
                u64::from_be_bytes(data[off + 8..off + 16].try_into().unwrap()) as usize,
            ),
            0 => (8, end - off),
            s => (8, s),
        };
        if size < hdr || off + size > end {
            break;
        }
        out.push((kind, off + hdr, off + size));
        off += size;
    }
    out
}

/// Per-track media timescales from the init's `moov/trak/mdia/mdhd`, in track
/// order (matches the per-moof `traf` order ffmpeg writes).
fn init_timescales(init: &[u8]) -> Vec<u32> {
    let mut out = Vec::new();
    for (kind, bs, be) in boxes(init, 0, init.len()) {
        if &kind != b"moov" {
            continue;
        }
        for (tk, tbs, tbe) in boxes(init, bs, be) {
            if &tk != b"trak" {
                continue;
            }
            for (mk, mbs, mbe) in boxes(init, tbs, tbe) {
                if &mk != b"mdia" {
                    continue;
                }
                for (hk, hbs, _) in boxes(init, mbs, mbe) {
                    if &hk == b"mdhd" {
                        let off = hbs + 4 + if init[hbs] == 1 { 16 } else { 8 };
                        out.push(u32::from_be_bytes(init[off..off + 4].try_into().unwrap()));
                    }
                }
            }
        }
    }
    out
}

/// Sample-accurate per-track timing of a media segment: for each track id,
/// the earliest `tfdt` and the summed sample durations across ALL its moofs
/// (a 6 s segment usually carries several fragments).
fn frag_timing(seg: &[u8]) -> Vec<(u64, u64)> {
    use std::collections::BTreeMap;
    let mut acc: BTreeMap<u32, (u64, u64)> = BTreeMap::new();
    for (kind, bs, be) in boxes(seg, 0, seg.len()) {
        if &kind != b"moof" {
            continue;
        }
        for (tk, tbs, tbe) in boxes(seg, bs, be) {
            if &tk != b"traf" {
                continue;
            }
            let (mut tid, mut base, mut default_dur, mut dur_sum) = (0u32, u64::MAX, 0u64, 0u64);
            for (ck, cbs, _) in boxes(seg, tbs, tbe) {
                let flags = u32::from_be_bytes(seg[cbs..cbs + 4].try_into().unwrap()) & 0x00FF_FFFF;
                match &ck {
                    b"tfhd" => {
                        let mut p = cbs + 4;
                        tid = u32::from_be_bytes(seg[p..p + 4].try_into().unwrap());
                        p += 4;
                        if flags & 0x1 != 0 {
                            p += 8; // base-data-offset
                        }
                        if flags & 0x2 != 0 {
                            p += 4; // sample-description-index
                        }
                        if flags & 0x8 != 0 {
                            default_dur =
                                u32::from_be_bytes(seg[p..p + 4].try_into().unwrap()) as u64;
                        }
                    }
                    b"tfdt" => {
                        base = if seg[cbs] == 1 {
                            u64::from_be_bytes(seg[cbs + 4..cbs + 12].try_into().unwrap())
                        } else {
                            u32::from_be_bytes(seg[cbs + 4..cbs + 8].try_into().unwrap()) as u64
                        };
                    }
                    b"trun" => {
                        let mut p = cbs + 4;
                        let count = u32::from_be_bytes(seg[p..p + 4].try_into().unwrap());
                        p += 4;
                        if flags & 0x1 != 0 {
                            p += 4; // data-offset
                        }
                        if flags & 0x4 != 0 {
                            p += 4; // first-sample-flags
                        }
                        if flags & 0x100 != 0 {
                            for _ in 0..count {
                                dur_sum +=
                                    u32::from_be_bytes(seg[p..p + 4].try_into().unwrap()) as u64;
                                p += 4;
                                p += 4 * u32::count_ones(flags & 0xE00) as usize;
                            }
                        } else {
                            dur_sum += default_dur * count as u64;
                        }
                    }
                    _ => {}
                }
            }
            let e = acc.entry(tid).or_insert((u64::MAX, 0));
            e.0 = e.0.min(base);
            e.1 += dur_sum;
        }
    }
    acc.into_values().collect()
}

#[actix_web::test]
#[ignore = "requires ffmpeg (libvpx-vp9 + libopus) on PATH"]
async fn vp9_fmp4_path_serves_seekable_hls() {
    if !ffmpeg_ok() {
        eprintln!("skipping: ffmpeg not found");
        return;
    }
    let td = TempDir::new().unwrap();
    let clip = make_clip(td.path());
    let (state, token) = seed(clip, &td.path().join("cache")).await;
    let app = test::init_service(App::new().app_data(state).configure(hls::register)).await;

    // 1. Master advertises vp09 + routes to the VP9 variant.
    let master = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/videos/42/vp9/master.m3u8?api_key={token}"))
            .to_request(),
    )
    .await;
    let master = std::str::from_utf8(&master).unwrap();
    assert!(
        master.contains("vp09"),
        "master must advertise vp09:\n{master}"
    );
    assert!(
        master.contains("/videos/42/vp9/main.m3u8"),
        "master must route to the VP9 variant:\n{master}"
    );

    // 2. Variant is a VOD fMP4 playlist: EXT-X-MAP init + .m4s segments.
    let variant = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/videos/42/vp9/main.m3u8?api_key={token}"))
            .to_request(),
    )
    .await;
    let variant = std::str::from_utf8(&variant).unwrap();
    assert!(variant.contains("#EXT-X-VERSION:7"), "{variant}");
    assert!(
        variant.contains("#EXT-X-MAP:URI=\"/videos/42/vp9/init.mp4"),
        "variant must declare the fMP4 init:\n{variant}"
    );
    assert!(variant.contains("/videos/42/vp9/0.m4s"), "{variant}");
    assert!(
        variant.contains("/videos/42/vp9/2.m4s"),
        "15s/6s ⇒ ≥3 segs:\n{variant}"
    );

    // 3. init.mp4 is ftyp+moov, no moof.
    let init = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/videos/42/vp9/init.mp4?api_key={token}"))
            .to_request(),
    )
    .await;
    assert!(
        has_box(&init, b"ftyp") && has_box(&init, b"moov"),
        "init needs ftyp+moov"
    );
    assert!(!has_box(&init, b"moof"), "init must not contain moof");

    // 4. Segment 0 is moof-only media (no moov/ftyp), tfdt at 0.
    let seg0 = test::call_and_read_body(
        &app,
        test::TestRequest::get()
            .uri(&format!("/videos/42/vp9/0.m4s?api_key={token}"))
            .to_request(),
    )
    .await;
    assert!(
        has_box(&seg0, b"moof") && has_box(&seg0, b"mdat"),
        "seg0 needs moof+mdat"
    );
    assert!(
        !has_box(&seg0, b"moov"),
        "media segment must not carry moov"
    );
    assert!(!has_box(&seg0, b"mfra"), "stale mfra must be stripped");
    // A 6 s segment can hold >1 fragment; tfdts interleave [video, audio, …]
    // per fragment (moov track order). The FIRST fragment sits at the origin.
    let seg0_tfdts = tfdt_values(&seg0);
    assert!(
        seg0_tfdts.len() >= 2,
        "seg0 needs ≥1 fragment: {seg0_tfdts:?}"
    );
    assert_eq!(
        (seg0_tfdts[0], seg0_tfdts[1]),
        (0, 0),
        "seg0's first fragment starts at tfdt 0: {seg0_tfdts:?}"
    );

    // 5. Segments are SOURCE-anchored and tile per track. This is the A/V
    //    drift regression guard: an exact-6.0-grid tfdt would pass a naive
    //    "seg2 starts at 12 s" check while re-timing video against audio by
    //    a few ms per segment; asserting butt-joins on the true source
    //    timeline pins the correct behaviour.
    let timescales = init_timescales(&init);
    assert_eq!(
        timescales.len(),
        2,
        "expect video+audio tracks: {timescales:?}"
    );
    let (vts, ats) = (timescales[0] as f64, timescales[1] as f64);
    let mut segs = Vec::new();
    for n in 0..3u32 {
        let body = test::call_and_read_body(
            &app,
            test::TestRequest::get()
                .uri(&format!("/videos/42/vp9/{n}.m4s?api_key={token}"))
                .to_request(),
        )
        .await;
        let timing = frag_timing(&body);
        assert_eq!(timing.len(), 2, "seg{n} needs video+audio: {timing:?}");
        segs.push(timing);
    }
    for (n, timing) in segs.iter().enumerate() {
        let (v_start, a_start) = (timing[0].0 as f64 / vts, timing[1].0 as f64 / ats);
        let want = n as f64 * 6.0;
        // Video anchors at the first source frame ≥ the boundary — within
        // one frame duration (24 fps ⇒ ~42 ms) of N·6, never before it.
        assert!(
            (want - 0.001..want + 0.1).contains(&v_start),
            "seg{n} video start {v_start:.4} not source-anchored near {want}"
        );
        // Audio anchors at the boundary minus the opus preskip (312/48000 ≈
        // 6.5 ms); segment 0 is clamped to exactly 0.
        assert!(
            (want - 0.01..want + 0.001).contains(&a_start),
            "seg{n} audio start {a_start:.4} not source-anchored near {want}"
        );
    }
    for n in 0..2usize {
        let v_gap = (segs[n + 1][0].0 as f64 - (segs[n][0].0 + segs[n][0].1) as f64) / vts * 1000.0;
        let a_gap = (segs[n + 1][1].0 as f64 - (segs[n][1].0 + segs[n][1].1) as f64) / ats * 1000.0;
        // Video must butt-join: no dropped boundary frame (gap ≤ ~half a
        // frame) and no more than one duplicated frame of overlap.
        assert!(
            (-45.0..=20.0).contains(&v_gap),
            "seg{n}→{}: video tiling gap {v_gap:.1} ms (want ≈0: >0 drops frames → stutter, \
             <-45 duplicates >1 frame)",
            n + 1
        );
        // Audio overlaps by exactly the opus preskip (constant, never grows).
        // The 0→1 boundary carries one extra preskip: segment 0's clamped
        // tfdt (-6.5 ms → 0) pushes its end 6.5 ms late, so ≈ -13 ms there.
        assert!(
            (-15.0..=1.0).contains(&a_gap),
            "seg{n}→{}: audio tiling gap {a_gap:.1} ms (want ≈-6.5 preskip overlap)",
            n + 1
        );
    }
}
