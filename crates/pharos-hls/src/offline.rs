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
        .into_iter()
        .chain(media.segments.into_iter().map(|s| s.uri))
        .collect();
    Ok(MediaPlan {
        files,
        duration_ms,
        short_tail,
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
}
