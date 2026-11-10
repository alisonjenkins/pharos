//! Spec 028 FR-017 / V532 (B604): `stream.ass` for the styled slot compiles
//! the stored plan per `?tier=&effects=`, every other subtitle route refuses
//! the slot, and each delivery is counted with `source="generated"`.

use actix_web::{http::header::HeaderMap, test, web};
use pharos_cache::SubtitleCache;
use pharos_core::{
    LibraryKind, LibraryStore as _, MediaItem, MediaKind, MediaStore as _, SubtitleStyling,
    SubtitleStylingStore as _,
};
use pharos_jellyfin_api::dto::sidecar_base_index;
use pharos_server::state::AppState;
use pharos_server::styled_track::styled_index;

use crate::common::{build_app, seed_rich, Fixture};

const ITEM: u64 = 320;
const SCRIPT: &str = "[Script Info]\nScriptType: v4.00+\n\n[Events]\n\
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
Dialogue: 0,0:00:01.00,0:00:02.50,Default,,0,0,0,,Hello there\n\
Dialogue: 0,0:00:03.00,0:00:04.50,Default,,0,0,0,,{\\i1}Second line{\\i0}\n";
const SRT: &str = "1\n00:00:01,000 --> 00:00:02,500\nHello from srt\n\n\
2\n00:00:03,000 --> 00:00:04,500\nSecond srt line\n";

struct Rig {
    fx: Fixture,
    state: web::Data<AppState>,
    item: MediaItem,
    source_index: u32,
    styled_index: u32,
    _dir: tempfile::TempDir,
}

fn signature(path: &std::path::Path) -> (i64, u64) {
    let m = std::fs::metadata(path).unwrap();
    let secs = m
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    (i64::try_from(secs).unwrap(), m.len())
}

/// An item with one sidecar (`sidecar_name`, holding `body`), the sidecar
/// as the plan's source, and the styling row stored.
async fn rig(sidecar_name: &str, body: &str) -> Rig {
    let fx = seed_rich().await;
    let dir = tempfile::tempdir().unwrap();
    fx.state
        .stores
        .upsert_library(
            "Scratch",
            dir.path().to_str().unwrap(),
            LibraryKind::Movies,
            &"d".repeat(32),
        )
        .await
        .unwrap();
    std::fs::write(dir.path().join("Film.mkv"), b"x").unwrap();
    let sidecar = dir.path().join(sidecar_name);
    std::fs::write(&sidecar, body).unwrap();
    let item = MediaItem {
        id: ITEM,
        path: dir.path().join("Film.mkv"),
        title: "Film".into(),
        kind: MediaKind::Movie,
        ..MediaItem::default()
    };
    fx.state.stores.put(item.clone()).await.unwrap();
    let source_index = sidecar_base_index(&item.probe);
    let source = pharos_styling::source::read(body).unwrap();
    let mut plan = pharos_styling::direct(&source.lines, &Default::default(), &[]).plan;
    // A plan that styles line 0, so the tier and level change the script,
    // and gives line 1 a speaker, so speaker colours change it too.
    let shout = pharos_styling::LineEffect {
        line: 0,
        intent: pharos_styling::Intent::Shout,
        intensity: 3,
        placement: pharos_styling::Placement::Default,
        karaoke: pharos_styling::Karaoke::None,
        reason: "test".into(),
        static_only: false,
        syllables: None,
        speaker: None,
    };
    let spoken = pharos_styling::LineEffect {
        line: 1,
        intent: pharos_styling::Intent::Plain,
        intensity: 0,
        speaker: Some(0),
        ..shout.clone()
    };
    plan.lines = vec![shout, spoken];
    let (source_mtime, source_size) = signature(&item.path);
    let (sidecar_mtime, sidecar_size) = signature(&sidecar);
    fx.state
        .stores
        .put_subtitle_styling(SubtitleStyling {
            item_id: ITEM,
            source_index,
            source_mtime,
            source_size,
            sidecar_mtime: Some(sidecar_mtime),
            sidecar_size: Some(sidecar_size),
            director_version: pharos_styling::DIRECTOR_VERSION,
            outcome: "styled".into(),
            plan_json: Some(serde_json::to_string(&plan).unwrap()),
            styled_at: 1_700_000_100,
        })
        .await
        .unwrap();
    let state = web::Data::new(
        AppState::new(fx.state.stores.clone(), "t".into())
            .with_subtitle_cache(SubtitleCache::new(1 << 20, 64)),
    );
    Rig {
        fx,
        state,
        styled_index: styled_index(&item.probe, 1),
        item,
        source_index,
        _dir: dir,
    }
}

async fn get(rig: &Rig, uri: &str) -> (u16, HeaderMap, Vec<u8>) {
    get_from(&rig.state, &rig.fx.admin_token, uri).await
}

async fn get_from(
    state: &web::Data<AppState>,
    token: &str,
    uri: &str,
) -> (u16, HeaderMap, Vec<u8>) {
    let app = test::init_service(build_app(state.clone())).await;
    let req = test::TestRequest::get()
        .uri(uri)
        .insert_header(("X-Emby-Token", token))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    (status, headers, test::read_body(resp).await.to_vec())
}

fn ass_uri(rig: &Rig, query: &str) -> String {
    format!(
        "/videos/{ITEM}/subtitles/{}/stream.ass{query}",
        rig.styled_index
    )
}

#[actix_web::test]
async fn a_tier_and_level_compile_the_plan_into_a_parsing_script() {
    let rig = rig("Film.eng.ass", SCRIPT).await;
    let (status, headers, body) = get(&rig, &ass_uri(&rig, "?tier=desktop&effects=full")).await;
    assert_eq!(status, 200);
    assert_eq!(
        headers.get("content-type").unwrap(),
        "text/x-ssa; charset=utf-8"
    );
    assert_eq!(headers.get("x-pharos-subtitle-tier").unwrap(), "desktop");
    assert_eq!(headers.get("cache-control").unwrap(), "private, no-cache");
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("[Events]"), "{text}");
    assert!(text.contains("Hello there"), "{text}");
    assert_eq!(
        pharos_subs::Script::parse(&text).stats.bad_lines,
        0,
        "{text}"
    );
}

#[actix_web::test]
async fn the_default_is_tier_tv_and_effects_subtle() {
    let rig = rig("Film.eng.ass", SCRIPT).await;
    let (status, headers, plain) = get(&rig, &ass_uri(&rig, "")).await;
    assert_eq!(status, 200);
    assert_eq!(headers.get("x-pharos-subtitle-tier").unwrap(), "tv");
    let (_, _, named) = get(&rig, &ass_uri(&rig, "?tier=tv&effects=subtle")).await;
    assert_eq!(plain, named, "the defaults are tv and subtle");
    let (status, headers, empty_tier) = get(&rig, &ass_uri(&rig, "?tier=&effects=subtle")).await;
    assert_eq!(status, 200, "an empty tier counts as absent");
    assert_eq!(headers.get("x-pharos-subtitle-tier").unwrap(), "tv");
    assert_eq!(plain, empty_tier);
    let (_, _, full) = get(&rig, &ass_uri(&rig, "?tier=desktop&effects=full")).await;
    let (_, _, off) = get(&rig, &ass_uri(&rig, "?tier=low&effects=off")).await;
    assert_ne!(off, full, "the level and tier reach the compiler");
}

#[actix_web::test]
async fn an_unknown_tier_or_level_is_a_400_and_no_plan_a_404() {
    let rig = rig("Film.eng.ass", SCRIPT).await;
    assert_eq!(get(&rig, &ass_uri(&rig, "?tier=huge")).await.0, 400);
    assert_eq!(get(&rig, &ass_uri(&rig, "?effects=loud")).await.0, 400);
    // The video replaced after the plan: the plan no longer applies.
    std::fs::write(&rig.item.path, b"another video").unwrap();
    assert_eq!(get(&rig, &ass_uri(&rig, "")).await.0, 404);
}

/// B852 (V802): `?size=` scales the dialogue to the viewer's subtitle size, is
/// cached apart from the other sizes, and 100 is the default.
#[actix_web::test]
async fn a_size_scales_the_dialogue_style() {
    let rig = rig("Film.eng.ass", SCRIPT).await;
    let default_style = |body: &[u8]| {
        String::from_utf8(body.to_vec())
            .unwrap()
            .lines()
            .find(|l| l.starts_with("Style: Default,"))
            .map(str::to_owned)
            .unwrap()
    };
    let (_, _, plain) = get(&rig, &ass_uri(&rig, "")).await;
    let (_, _, hundred) = get(&rig, &ass_uri(&rig, "?size=100")).await;
    assert_eq!(plain, hundred, "100 is the default");
    let (status, _, large) = get(&rig, &ass_uri(&rig, "?size=150")).await;
    assert_eq!(status, 200);
    assert!(default_style(&plain).starts_with("Style: Default,Liberation Sans,56,"));
    assert!(default_style(&large).starts_with("Style: Default,Liberation Sans,84,"));
    let (_, _, again) = get(&rig, &ass_uri(&rig, "")).await;
    assert_eq!(
        again, plain,
        "a sized script does not take the default's cache slot"
    );
    assert_eq!(get(&rig, &ass_uri(&rig, "?size=110")).await.0, 400);
    assert_eq!(get(&rig, &ass_uri(&rig, "?size=big")).await.0, 400);
}

/// `?speakers=1` draws a speaker's lines in their colour and is cached apart
/// from the script without; off is the default.
#[actix_web::test]
async fn speaker_colours_are_a_switch_of_their_own() {
    let rig = rig("Film.eng.ass", SCRIPT).await;
    let speaker_lines = |body: &[u8]| {
        String::from_utf8(body.to_vec())
            .unwrap()
            .lines()
            .filter(|l| l.starts_with("Style: Speaker1,") || l.contains(",Speaker1,"))
            .count()
    };
    let (_, _, off) = get(&rig, &ass_uri(&rig, "")).await;
    let (_, _, zero) = get(&rig, &ass_uri(&rig, "?speakers=0")).await;
    assert_eq!(off, zero, "off is the default");
    assert_eq!(speaker_lines(&off), 0);
    let (status, _, on) = get(&rig, &ass_uri(&rig, "?speakers=1")).await;
    assert_eq!(status, 200);
    // Its style and the one line it speaks.
    assert_eq!(speaker_lines(&on), 2, "{}", String::from_utf8_lossy(&on));
    let (_, _, again) = get(&rig, &ass_uri(&rig, "")).await;
    assert_eq!(again, off, "the coloured script has its own cache slot");
    assert_eq!(get(&rig, &ass_uri(&rig, "?speakers=yes")).await.0, 400);
}

/// V532: the same request twice gives the same bytes, from the cache and from
/// a cold start.
#[actix_web::test]
async fn the_same_request_gives_identical_bytes_cached_or_cold() {
    let rig = rig("Film.eng.ass", SCRIPT).await;
    let uri = ass_uri(&rig, "?tier=tv&effects=full");
    let (_, _, cold) = get(&rig, &uri).await;
    let (_, _, warm) = get(&rig, &uri).await;
    assert_eq!(cold, warm);
    let fresh = web::Data::new(
        AppState::new(rig.fx.state.stores.clone(), "t".into())
            .with_subtitle_cache(SubtitleCache::new(1 << 20, 64)),
    );
    assert_eq!(get_from(&fresh, &rig.fx.admin_token, &uri).await.2, cold);
}

#[actix_web::test]
async fn an_srt_source_is_styled_too() {
    let rig = rig("Film.eng.srt", SRT).await;
    let (status, _, body) = get(&rig, &ass_uri(&rig, "?effects=full")).await;
    assert_eq!(status, 200);
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("Hello from srt"), "{text}");
    assert!(text.contains("[Events]"), "{text}");
}

#[actix_web::test]
async fn the_plain_routes_refuse_the_styled_index() {
    let rig = rig("Film.eng.ass", SCRIPT).await;
    let i = rig.styled_index;
    for tail in ["stream.vtt", "stream.js", "subtitles.srt", "fonts/5"] {
        let (status, ..) = get(&rig, &format!("/videos/{ITEM}/subtitles/{i}/{tail}")).await;
        assert_eq!(status, 404, "{tail}");
    }
    // The refusal names the slot rather than a missing sidecar.
    let (_, _, body) = get(&rig, &format!("/videos/{ITEM}/subtitles/{i}/stream.vtt")).await;
    assert!(String::from_utf8_lossy(&body).contains("styled"));
    // The source track still serves.
    let s = rig.source_index;
    assert_eq!(
        get(&rig, &format!("/videos/{ITEM}/subtitles/{s}/stream.ass"))
            .await
            .0,
        200
    );
}

#[actix_web::test]
async fn deliveries_are_counted_as_generated() {
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = pharos_test_metrics::snapshotter(&recorder);
    let _metrics = metrics::set_default_local_recorder(&recorder);
    let rig = rig("Film.eng.ass", SCRIPT).await;
    assert_eq!(get(&rig, &ass_uri(&rig, "?tier=low")).await.0, 200);
    assert_eq!(get(&rig, &ass_uri(&rig, "?tier=nope")).await.0, 400);
    let i = rig.styled_index;
    assert_eq!(
        get(&rig, &format!("/videos/{ITEM}/subtitles/{i}/stream.vtt"))
            .await
            .0,
        404
    );
    let count = |format: &str, outcome: &str| -> u64 {
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| {
                let key = k.key();
                key.name() == "pharos_subtitle_delivery_total"
                    && [
                        ("format", format),
                        ("source", "generated"),
                        ("codec_class", "styled"),
                        ("outcome", outcome),
                    ]
                    .iter()
                    .all(|(lk, lv)| key.labels().any(|l| l.key() == *lk && l.value() == *lv))
            })
            .map(|(_, _, _, v)| match v {
                metrics_util::debugging::DebugValue::Counter(n) => n,
                _ => 0,
            })
            .sum()
    };
    assert_eq!(count("ass", "served"), 1);
    assert_eq!(count("ass", "refused"), 1);
    assert_eq!(count("vtt", "refused"), 1);
}
