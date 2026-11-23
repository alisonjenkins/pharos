//! Offline downloads (spec 048): which URLs make up one stream, and how big
//! it is. Pure, so the web app and the phone app plan a download alike; each
//! host fetches and stores the URLs under their exact text.

use crate::playlist::{parse_master, parse_media, ParseError};
use crate::uri::resolve;

/// What a master playlist leads to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterPlan {
    /// Every media playlist the player may open: each variant's, then each
    /// audio rendition's, resolved against the master's URL, no repeats.
    pub media_playlists: Vec<String>,
    /// The highest variant `BANDWIDTH`, bits per second, for the estimate.
    pub bandwidth_bps: u64,
}

/// The media playlists `text` (the master at `master_url`) names.
pub fn plan_master(master_url: &str, text: &str) -> Result<MasterPlan, ParseError> {
    let master = parse_master(text, master_url)?;
    let mut media_playlists: Vec<String> = Vec::new();
    let uris = master
        .variants
        .iter()
        .map(|v| v.uri.clone())
        .chain(master.audio.iter().filter_map(|r| r.uri.clone()));
    for uri in uris {
        if !media_playlists.contains(&uri) {
            media_playlists.push(uri);
        }
    }
    let bandwidth_bps = master
        .variants
        .iter()
        .map(|v| v.bandwidth)
        .max()
        .unwrap_or(0);
    Ok(MasterPlan {
        media_playlists,
        bandwidth_bps,
    })
}

/// What one media playlist leads to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaPlan {
    /// The init segment (if any) then every media segment, resolved.
    pub files: Vec<String>,
    /// The playlist's length.
    pub duration_ms: u64,
    /// The last segment, when it is under [`SHORT_TAIL_MS`] and no other
    /// segment names its URI: a sliver the server's grid leaves at the very
    /// end, which an encoder can fail to fill. A host may go without it; the
    /// stream then ends a frame early. A URI another segment needs is never
    /// one, so its failed fetch is never excused.
    pub short_tail: Option<String>,
    /// The init segments among `files`: compared byte for byte when a
    /// download carries on under a renewed play session (ADR 0148).
    pub inits: Vec<String>,
}

/// A last segment shorter than this is a short tail (see [`MediaPlan`]).
pub const SHORT_TAIL_MS: u64 = 1000;

/// The files the media playlist `text` (at `url`) names.
pub fn plan_media(url: &str, text: &str) -> Result<MediaPlan, ParseError> {
    let media = parse_media(text, url)?;
    let duration_ms = media
        .segments
        .last()
        .map_or(0, |s| s.start_ms.saturating_add(s.duration_ms));
    // One file per URI less its fragment, as a host fetches and keys them:
    // a segment that differs only by `#...` names the same file.
    let short_tail = media
        .segments
        .last()
        .filter(|s| s.duration_ms < SHORT_TAIL_MS)
        .filter(|s| {
            media
                .segments
                .iter()
                .filter(|o| without_fragment(&o.uri) == without_fragment(&s.uri))
                .count()
                == 1
        })
        .map(|s| s.uri.clone());
    // A multi-part film names one `EXT-X-MAP` per part (after each
    // discontinuity), and the parser keeps only the last: list them all.
    let mut inits: Vec<String> = text
        .lines()
        .filter_map(|line| line.trim().strip_prefix("#EXT-X-MAP:"))
        .filter_map(map_uri)
        .map(|uri| resolve(url, uri))
        .collect();
    if inits.is_empty() {
        inits.extend(media.init_uri);
    }
    inits.dedup();
    let files = inits
        .iter()
        .cloned()
        .chain(media.segments.into_iter().map(|s| s.uri))
        .collect();
    Ok(MediaPlan {
        files,
        duration_ms,
        short_tail,
        inits,
    })
}

/// `uri` without its fragment: what a fetch of it asks for.
fn without_fragment(uri: &str) -> &str {
    uri.split_once('#').map_or(uri, |(file, _)| file)
}

/// The `URI="..."` value of an `EXT-X-MAP` tag's attributes.
fn map_uri(attributes: &str) -> Option<&str> {
    let start = attributes.find("URI=\"")? + 5;
    let rest = &attributes[start..];
    rest.find('"').map(|end| &rest[..end])
}

/// The bytes a stream of `bandwidth_bps` lasting `duration_ms` takes.
pub fn estimate_bytes(bandwidth_bps: u64, duration_ms: u64) -> u64 {
    let bits = u128::from(bandwidth_bps) * u128::from(duration_ms) / 1000;
    u64::try_from(bits / 8).unwrap_or(u64::MAX)
}

/// The query key that carries a playback's capability (ADR 0148).
pub const SESSION_KEY: &str = "PlaySessionId";

/// The `PlaySessionId` value `url`'s query carries, if any (key matched
/// without regard to case, as the server does).
pub fn play_session(url: &str) -> Option<&str> {
    // The fragment ends the URL for a server: a `?` inside it opens no
    // query, so a session parameter cannot be supplied from there.
    let url = without_fragment(url);
    let (_, query) = url.split_once('?')?;
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k.eq_ignore_ascii_case(SESSION_KEY) && !v.is_empty()).then_some(v)
    })
}

/// `url` with every `PlaySessionId` value replaced by `session`: the URL a
/// stored key is fetched from under a renewed play session (ADR 0148). The
/// rest of the URL is left byte for byte, so the stored key never changes.
pub fn rebind(url: &str, session: &str) -> String {
    swap_session(url, |_| session.to_owned())
}

/// `url` with its `PlaySessionId` value hidden, for log lines: the id is a
/// capability, everything else is not.
pub fn redact_session(url: &str) -> String {
    swap_session(url, |_| "~".to_owned())
}

fn swap_session(url: &str, with: impl Fn(&str) -> String) -> String {
    // The fragment is cut off first: a `?` inside it is not a query.
    let (url, fragment) = match url.split_once('#') {
        Some((u, f)) => (u, Some(f)),
        None => (url, None),
    };
    let Some((head, query)) = url.split_once('?') else {
        return match fragment {
            Some(f) => format!("{url}#{f}"),
            None => url.to_owned(),
        };
    };
    let query = query
        .split('&')
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) if k.eq_ignore_ascii_case(SESSION_KEY) => format!("{k}={}", with(v)),
            _ => kv.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("&");
    match fragment {
        Some(f) => format!("{head}?{query}#{f}"),
        None => format!("{head}?{query}"),
    }
}

/// Whether a playlist fetched under the renewed session `live` says what
/// the one stored under `kept` says, once its session ids are put back:
/// the same segments, durations and inits. `Err` names the first line that
/// differs (1-based), with the session hidden.
pub fn same_playlist(
    stored: &str,
    renewed: &str,
    kept: &str,
    live: &str,
) -> Result<(), PlanMismatch> {
    let renewed: Vec<String> = renewed
        .lines()
        .map(|line| restore_session(line, live, kept))
        .collect();
    let (mut a, mut b) = (stored.lines(), renewed.iter().map(String::as_str));
    let mut at = 0;
    loop {
        at += 1;
        match (a.next(), b.next()) {
            (None, None) => return Ok(()),
            (x, y) if x == y => {}
            (x, y) => {
                return Err(PlanMismatch {
                    at,
                    kept: x.map(redact_session).unwrap_or_default(),
                    renewed: y.map(redact_session).unwrap_or_default(),
                })
            }
        }
    }
}

/// `line` with each `PlaySessionId=<live>` parameter put back to `kept`.
/// Only that parameter's value moves: a path, another parameter or any
/// other text that happens to contain `live` is left as it is, so a changed
/// path still shows up as a difference.
fn restore_session(line: &str, live: &str, kept: &str) -> String {
    if live.is_empty() {
        return line.to_owned();
    }
    let key = format!("{SESSION_KEY}=").to_ascii_lowercase();
    let lower = line.to_ascii_lowercase();
    let mut out = String::with_capacity(line.len());
    let mut at = 0;
    while let Some(found) = lower[at..].find(&key) {
        let key_at = at + found;
        let value_at = key_at + key.len();
        let after_delim = key_at > 0 && matches!(line.as_bytes()[key_at - 1], b'?' | b'&');
        let value_end = line[value_at..]
            .find(['&', '#', '"', ' ', ','])
            .map_or(line.len(), |n| value_at + n);
        out.push_str(&line[at..value_at]);
        if after_delim && &line[value_at..value_end] == live {
            out.push_str(kept);
        } else {
            out.push_str(&line[value_at..value_end]);
        }
        at = value_end;
    }
    out.push_str(&line[at..]);
    out
}

/// Where a renewed stream's plan first parts from the stored one, with the
/// play session hidden in both URLs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanMismatch {
    /// The position of the first difference in the file list.
    pub at: usize,
    /// The stored plan's URL there (empty past its end).
    pub kept: String,
    /// The renewed plan's URL there (empty past its end).
    pub renewed: String,
}

impl std::fmt::Display for PlanMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "file {} differs: kept {:?}, renewed {:?}",
            self.at, self.kept, self.renewed
        )
    }
}

/// Whether `renewed` (a plan made under play session `session`) names the
/// same files as `kept` once each kept key is rebound to `session`: only
/// then may the files already stored be kept (ADR 0148).
pub fn same_plan(kept: &[String], renewed: &[String], session: &str) -> Result<(), PlanMismatch> {
    let len = kept.len().max(renewed.len());
    for at in 0..len {
        let k = kept.get(at).map(|k| rebind(k, session));
        let r = renewed.get(at);
        if k.as_ref() != r {
            return Err(PlanMismatch {
                at,
                kept: k.as_deref().map(redact_session).unwrap_or_default(),
                renewed: r.map(|r| redact_session(r)).unwrap_or_default(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // Panicking on unexpected parse results is the intended failure mode in unit tests.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const MASTER: &str = "#EXTM3U\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"English\",DEFAULT=YES,URI=\"/videos/9/vp9/audio.m3u8?PlaySessionId=p\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=2128000,CODECS=\"avc1.64001f\",AUDIO=\"a\"\n\
        /videos/9/h264cmaf/main.m3u8?PlaySessionId=p\n";

    const MEDIA: &str = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:6\n\
        #EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MAP:URI=\"init.mp4?PlaySessionId=p\"\n\
        #EXTINF:6.0,\n0.m4s?PlaySessionId=p\n#EXTINF:4.5,\n1.m4s?PlaySessionId=p\n#EXT-X-ENDLIST\n";

    #[test]
    fn a_master_names_its_variant_and_audio_playlists_with_their_queries() {
        let plan = plan_master("https://h/videos/9/master.m3u8?PlaySessionId=p", MASTER).unwrap();
        assert_eq!(
            plan.media_playlists,
            [
                "https://h/videos/9/h264cmaf/main.m3u8?PlaySessionId=p",
                "https://h/videos/9/vp9/audio.m3u8?PlaySessionId=p",
            ]
        );
        assert_eq!(plan.bandwidth_bps, 2_128_000);
    }

    #[test]
    fn a_media_playlist_names_its_init_then_its_segments() {
        let plan = plan_media(
            "https://h/videos/9/h264cmaf/main.m3u8?PlaySessionId=p",
            MEDIA,
        )
        .unwrap();
        assert_eq!(
            plan.files,
            [
                "https://h/videos/9/h264cmaf/init.mp4?PlaySessionId=p",
                "https://h/videos/9/h264cmaf/0.m4s?PlaySessionId=p",
                "https://h/videos/9/h264cmaf/1.m4s?PlaySessionId=p",
            ]
        );
        assert_eq!(plan.duration_ms, 10_500);
        assert_eq!(plan.short_tail, None);
        assert_eq!(
            plan.inits,
            ["https://h/videos/9/h264cmaf/init.mp4?PlaySessionId=p"]
        );
    }

    /// A multi-part film's playlist gives each part its own init after a
    /// discontinuity; every one is planned.
    #[test]
    fn every_part_init_of_a_multi_part_playlist_is_planned() {
        let text = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:6\n\
            #EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MAP:URI=\"init-a.mp4?p=1\"\n\
            #EXTINF:6.0,\na0.m4s?p=1\n#EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"init-b.mp4?p=1\"\n\
            #EXTINF:6.0,\nb0.m4s?p=1\n#EXT-X-ENDLIST\n";
        let plan = plan_media("https://h/videos/9/h264cmaf/main.m3u8?p=1", text).unwrap();
        assert_eq!(
            plan.files,
            [
                "https://h/videos/9/h264cmaf/init-a.mp4?p=1",
                "https://h/videos/9/h264cmaf/init-b.mp4?p=1",
                "https://h/videos/9/h264cmaf/a0.m4s?p=1",
                "https://h/videos/9/h264cmaf/b0.m4s?p=1",
            ]
        );
    }

    #[test]
    fn a_last_segment_under_a_second_is_a_short_tail() {
        let text = MEDIA.replace("#EXTINF:4.5,", "#EXTINF:0.042,");
        let plan = plan_media("https://h/videos/9/h264cmaf/main.m3u8?p=1", &text).unwrap();
        assert_eq!(
            plan.short_tail.as_deref(),
            Some("https://h/videos/9/h264cmaf/1.m4s?PlaySessionId=p")
        );
    }

    /// CR review 5995575621 (kept_video_disk.rs:3431): a short last
    /// segment whose URI an earlier, longer segment also names is no short
    /// tail: that file is needed whole.
    #[test]
    fn a_short_last_segment_sharing_its_uri_is_no_short_tail() {
        let text = MEDIA
            .replace("#EXTINF:4.5,", "#EXTINF:0.042,")
            .replace("1.m4s?PlaySessionId=p", "0.m4s?PlaySessionId=p");
        let plan = plan_media("https://h/videos/9/h264cmaf/main.m3u8?p=1", &text).unwrap();
        assert_eq!(plan.short_tail, None);
    }

    /// CR 4646280204: a short last segment that differs from an earlier
    /// one only by its fragment names the same file: no short tail.
    #[test]
    fn a_short_last_segment_differing_only_by_fragment_is_no_short_tail() {
        let text = MEDIA
            .replace("#EXTINF:4.5,", "#EXTINF:0.042,")
            .replace("1.m4s?PlaySessionId=p", "0.m4s?PlaySessionId=p#t=6");
        let plan = plan_media("https://h/videos/9/h264cmaf/main.m3u8?p=1", &text).unwrap();
        assert_eq!(plan.short_tail, None);
    }

    #[test]
    fn an_hour_at_two_megabits_is_about_900_mb() {
        assert_eq!(estimate_bytes(2_000_000, 3_600_000), 900_000_000);
    }

    /// CR review 6018104500 (offline.rs:192): only the session parameter is
    /// put back. A renewed playlist whose path carries the live id, or whose
    /// other parameter does, differs from the stored one.
    #[test]
    fn only_the_session_value_is_put_back_in_a_renewed_playlist() {
        let kept = "#EXTM3U\n/p/seg0.m4s?PlaySessionId=p&x=1\n";
        let same = "#EXTM3U\n/p/seg0.m4s?PlaySessionId=q&x=1\n";
        assert_eq!(same_playlist(kept, same, "p", "q"), Ok(()));
        // The live id in a path is a different path, not a session.
        let kept = "#EXTM3U\n/p/seg0.m4s?PlaySessionId=p\n";
        let moved = "#EXTM3U\n/q/seg0.m4s?PlaySessionId=q\n";
        let err = same_playlist(kept, moved, "p", "q").unwrap_err();
        assert_eq!(err.at, 2, "a changed path passed as a session swap");
        // So is the id in another parameter.
        let other = "#EXTM3U\n/p/seg0.m4s?PlaySessionId=q&tag=q\n";
        let kept = "#EXTM3U\n/p/seg0.m4s?PlaySessionId=p&tag=p\n";
        assert!(same_playlist(kept, other, "p", "q").is_err());
        // A key's URI attribute is a URI reference too.
        let kept = "#EXT-X-KEY:METHOD=AES-128,URI=\"/k?PlaySessionId=p\"\n";
        let renewed = "#EXT-X-KEY:METHOD=AES-128,URI=\"/k?PlaySessionId=q\"\n";
        assert_eq!(same_playlist(kept, renewed, "p", "q"), Ok(()));
    }

    #[test]
    fn a_renewed_playlist_matches_once_its_session_is_put_back() {
        let kept = MEDIA;
        let renewed = MEDIA.replace("PlaySessionId=p", "PlaySessionId=q");
        assert_eq!(same_playlist(kept, &renewed, "p", "q"), Ok(()));
        let regridded = renewed.replace("#EXTINF:4.5,", "#EXTINF:4.0,");
        let err = same_playlist(kept, &regridded, "p", "q").unwrap_err();
        assert_eq!(err.at, 8);
        assert_eq!(
            (err.kept.as_str(), err.renewed.as_str()),
            ("#EXTINF:4.5,", "#EXTINF:4.0,")
        );
        let longer = format!("{renewed}#EXTINF:1.0,\n2.m4s?PlaySessionId=q\n");
        assert!(same_playlist(kept, &longer, "p", "q").is_err());
    }

    #[test]
    fn the_play_session_is_read_whatever_the_case_of_its_key() {
        assert_eq!(play_session("/v/0.m4s?a=1&PlaySessionId=p&b=2"), Some("p"));
        assert_eq!(play_session("/v/0.m4s?playsessionid=q"), Some("q"));
        assert_eq!(play_session("/v/0.m4s?PlaySessionId=&x=1"), None);
        assert_eq!(play_session("/v/0.m4s"), None);
    }

    /// Disarm check: a `rebind` that also touched `VideoBitrate` fails the
    /// first assertion.
    #[test]
    fn a_fragment_never_supplies_a_play_session() {
        assert_eq!(play_session("/v/0.m4s#x?PlaySessionId=evil"), None);
        assert_eq!(play_session("/v/0.m4s?a=1#x?PlaySessionId=evil"), None);
        assert_eq!(
            play_session("/v/0.m4s?PlaySessionId=p#t=1?PlaySessionId=evil"),
            Some("p")
        );
        // Rebinding and redacting leave a fragment's text as it is.
        assert_eq!(
            rebind("/v/0.m4s#x?PlaySessionId=a", "new"),
            "/v/0.m4s#x?PlaySessionId=a"
        );
        assert_eq!(
            redact_session("/v/0.m4s?PlaySessionId=a#x?PlaySessionId=b"),
            "/v/0.m4s?PlaySessionId=~#x?PlaySessionId=b"
        );
    }

    #[test]
    fn rebinding_swaps_only_the_play_session() {
        assert_eq!(
            rebind(
                "https://h/v/0.m4s?PlaySessionId=old&VideoBitrate=2000000#t",
                "new"
            ),
            "https://h/v/0.m4s?PlaySessionId=new&VideoBitrate=2000000#t"
        );
        assert_eq!(rebind("/v/0.m4s?x=1", "new"), "/v/0.m4s?x=1");
        assert_eq!(rebind("/v/0.m4s", "new"), "/v/0.m4s");
        assert_eq!(
            redact_session("/v/0.m4s?PlaySessionId=secret"),
            "/v/0.m4s?PlaySessionId=~"
        );
    }

    #[test]
    fn a_renewed_plan_with_the_same_files_matches() {
        let kept = vec![
            "/v/init.mp4?PlaySessionId=a".to_owned(),
            "/v/0.m4s?PlaySessionId=a".to_owned(),
        ];
        let renewed: Vec<String> = kept.iter().map(|k| rebind(k, "b")).collect();
        assert_eq!(same_plan(&kept, &renewed, "b"), Ok(()));
    }

    /// The first difference is named, with the session hidden, so the log
    /// line says what changed without leaking the capability.
    #[test]
    fn a_renewed_plan_that_differs_names_the_first_difference() {
        let kept = vec![
            "/v/init.mp4?PlaySessionId=a".to_owned(),
            "/v/0.m4s?PlaySessionId=a".to_owned(),
        ];
        let renewed = vec![
            "/v/init.mp4?PlaySessionId=b".to_owned(),
            "/v/0.m4s?PlaySessionId=b&VideoBitrate=1".to_owned(),
        ];
        let err = same_plan(&kept, &renewed, "b").unwrap_err();
        assert_eq!(err.at, 1);
        assert_eq!(err.kept, "/v/0.m4s?PlaySessionId=~");
        assert_eq!(err.renewed, "/v/0.m4s?PlaySessionId=~&VideoBitrate=1");
        let shorter = same_plan(&kept, &renewed[..1], "b").unwrap_err();
        assert_eq!((shorter.at, shorter.renewed.as_str()), (1, ""));
    }
}
