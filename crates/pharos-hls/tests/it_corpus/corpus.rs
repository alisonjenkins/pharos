//! Spec 031 T031-01: streams recorded from a seeded pharos (see
//! `tests/corpus/README.md`) read by the parser and the fMP4 reader the Rust
//! engine runs on, so a server change to what it emits shows up here before
//! it shows up in a browser.

use std::path::PathBuf;

use pharos_hls::fmp4::{decode_times, sample_entries, timescales, to_ms, CodecConfig, TrackKind};
use pharos_hls::playlist::{parse_master, parse_media, MediaPlaylist, Unsupported};
use pharos_hls::uri::resolve;

const BASE: &str = "http://pharos.test/videos/00000000000000000000000000000001/master.m3u8";

/// A recorded file, by the URI the playlists name it with.
fn read(stream: &str, uri: &str) -> Vec<u8> {
    // The path under `/videos/{item}/`, which is where the corpus keeps it.
    let path = uri
        .split('?')
        .next()
        .and_then(|p| {
            p.to_ascii_lowercase()
                .find("/videos/")
                .map(|i| &p[i + "/videos/".len()..])
        })
        .and_then(|p| p.split_once('/').map(|(_, rest)| rest))
        .unwrap_or_else(|| panic!("{uri} is not a recorded stream path"));
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/corpus")
        .join(stream)
        .join(path);
    std::fs::read(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()))
}

fn text(stream: &str, uri: &str) -> String {
    String::from_utf8(read(stream, uri)).unwrap()
}

fn media(stream: &str, uri: &str) -> MediaPlaylist {
    let pl = parse_media(&text(stream, uri), uri).unwrap();
    assert!(pl.vod && pl.ended, "{uri}: a VOD playlist with an end");
    assert!(pl.unsupported.is_empty(), "{uri}: {:?}", pl.unsupported);
    assert!(pl.init_uri.is_some(), "{uri}: fMP4 needs an init segment");
    let mut at = 0;
    for s in &pl.segments {
        assert_eq!(s.start_ms, at, "{uri}: segments run back to back");
        at += s.duration_ms;
    }
    assert!(at > 100_000, "{uri}: the whole 120 s clip, got {at} ms");
    pl
}

/// The init's codec, and that the first media segment starts at 0 in it.
fn check_track(stream: &str, pl: &MediaPlaylist) -> Option<CodecConfig> {
    let init = read(stream, pl.init_uri.as_deref().unwrap());
    let entries = sample_entries(&init).unwrap();
    assert_eq!(entries.len(), 1, "{stream}: one track per init");
    let scales = timescales(&init).unwrap();
    let first = pl.segments.first().unwrap();
    let times = decode_times(&read(stream, &first.uri)).unwrap();
    let (track, scale) = scales[0];
    let (_, t) = times.iter().find(|(id, _)| *id == track).copied().unwrap();
    assert!(
        to_ms(t, scale) < 100,
        "{stream}: segment 0 starts at {} ms",
        to_ms(t, scale)
    );
    entries.into_iter().next().unwrap().config
}

#[test]
fn the_browser_h264_master_is_a_cmaf_ladder_sharing_one_opus_group() {
    let stream = "h264-cmaf";
    let master = parse_master(
        &text(
            stream,
            "/videos/00000000000000000000000000000001/master.m3u8",
        ),
        BASE,
    )
    .unwrap();
    assert_eq!(master.version, Some(7));
    assert!(master.unsupported.is_empty(), "{:?}", master.unsupported);
    assert_eq!(master.variants.len(), 5, "source and four capped rungs");
    for v in &master.variants {
        assert_eq!(v.codecs, ["avc1.640028", "opus"]);
        assert_eq!(v.audio_group.as_deref(), Some("aud"));
    }
    let bandwidths: Vec<_> = master.variants.iter().map(|v| v.bandwidth).collect();
    assert!(
        bandwidths.windows(2).all(|w| w[0] > w[1]),
        "best first: {bandwidths:?}"
    );

    let ladder: Vec<_> = master
        .variants
        .iter()
        .map(|v| media(stream, &v.uri))
        .collect();
    // ABR switches rungs at segment boundaries, so every rung has the same ones.
    for pl in &ladder[1..] {
        let a: Vec<_> = pl
            .segments
            .iter()
            .map(|s| (s.start_ms, s.duration_ms))
            .collect();
        let b: Vec<_> = ladder[0]
            .segments
            .iter()
            .map(|s| (s.start_ms, s.duration_ms))
            .collect();
        assert_eq!(a, b);
    }
    for pl in [&ladder[0], ladder.last().unwrap()] {
        assert!(matches!(check_track(stream, pl), Some(CodecConfig::Avc(_))));
    }

    let audio = master.audio.iter().find(|r| r.default).unwrap();
    let audio_pl = media(stream, &resolve(BASE, audio.uri.as_deref().unwrap()));
    assert!(matches!(
        check_track(stream, &audio_pl),
        Some(CodecConfig::Opus(_))
    ));
}

#[test]
fn the_vp9_master_is_one_fmp4_rung_with_an_opus_group() {
    let stream = "vp9";
    let master = parse_master(
        &text(
            stream,
            "/videos/00000000000000000000000000000001/master.m3u8",
        ),
        BASE,
    )
    .unwrap();
    assert!(master.unsupported.is_empty(), "{:?}", master.unsupported);
    let [v] = master.variants.as_slice() else {
        panic!("one rung: {:?}", master.variants);
    };
    assert_eq!(v.codecs[0].split('.').next(), Some("vp09"));
    let pl = media(stream, &v.uri);
    assert!(matches!(
        check_track(stream, &pl),
        Some(CodecConfig::Vp9(_))
    ));
    let init = read(stream, pl.init_uri.as_deref().unwrap());
    let entry = &sample_entries(&init).unwrap()[0];
    assert!(matches!(
        entry.kind,
        TrackKind::Video {
            width: 320,
            height: 240
        }
    ));

    let audio = master.audio.iter().find(|r| r.default).unwrap();
    let uri = resolve(BASE, audio.uri.as_deref().unwrap());
    let audio_pl = media(stream, &uri);
    assert!(matches!(
        check_track(stream, &audio_pl),
        Some(CodecConfig::Opus(_))
    ));
}

// Music: the mp3 is served as muxed MPEG-TS, the source rung as MP3 and the
// capped rungs as AAC, with no video. The Rust engine is fMP4-only, so this
// is the stream it hands to hls.js.
#[test]
fn the_audio_only_master_is_a_muxed_ts_ladder_with_no_video() {
    let stream = "audio-only";
    let base = "http://pharos.test/videos/00000000000000003e29a6ec1c322978/master.m3u8";
    let master = parse_master(&text(stream, base), base).unwrap();
    assert_eq!(master.version, Some(3));
    assert!(master.audio.is_empty(), "the audio is in the variants");
    let codecs: Vec<_> = master.variants.iter().map(|v| v.codecs.join(",")).collect();
    assert_eq!(codecs[0], "mp4a.40.34", "the source rung is MP3");
    assert!(codecs[1..].iter().all(|c| c == "mp4a.40.2"), "{codecs:?}");
    assert!(master.variants.iter().all(|v| v.resolution.is_none()));
    for v in &master.variants {
        let pl = parse_media(&text(stream, &v.uri), &v.uri).unwrap();
        assert!(pl.vod && pl.ended, "{}", v.uri);
        assert_eq!(pl.init_uri, None, "{}: TS has no init segment", v.uri);
        assert!(pl
            .segments
            .iter()
            .all(|s| s.uri.split('?').next().unwrap().ends_with(".ts")));
        assert!(
            !pl.unsupported.contains(&Unsupported::ByteRange),
            "{}: {:?}",
            v.uri,
            pl.unsupported
        );
    }
    // A TS packet starts with the 0x47 sync byte, every 188 bytes.
    let first = parse_media(&text(stream, &master.variants[0].uri), base).unwrap();
    let ts = read(stream, &first.segments[0].uri);
    assert_eq!(ts.len() % 188, 0);
    assert!(ts.chunks(188).all(|p| p[0] == 0x47));
}
