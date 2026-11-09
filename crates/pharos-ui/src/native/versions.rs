//! A title's versions (V721): the files one film or episode can play from,
//! how the picker names them, and which one plays. Shared by every reason a
//! title has more than one file (a 3D edition, a cut, a second rip); a new
//! fact about a version extends [`version_label`].

use pharos_client::api::{ItemRef, Stereo, Version, VersionAudio};

use super::stereo::{packing_label, FrameRect};

/// The `localStorage` key holding the version last picked for `title`.
pub fn version_key(title: ItemRef) -> String {
    format!("pharos.version.{title}")
}

/// A version as the picker lists it: its 3D packing when it has one, its
/// edition name when that says more, then what tells its file apart
/// (`1080p · HDR10 · HEVC · EAC3 5.1 +1 · 4.2 GB`). A 2D version is not
/// marked 2D unless nothing else names it, so two copies of an episode read
/// by their files rather than as "2D" twice.
pub fn version_label(version: &Version) -> String {
    let mut parts = Vec::new();
    if let Some(s) = version.stereo {
        parts.push(format!("3D ({})", packing_label(s)));
    }
    let name = version.name.trim();
    // A 3D edition's name ("3D HSBS") only restates the packing named above.
    let restates_3d =
        version.stereo.is_some() && name.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("3d"));
    if !name.is_empty() && !restates_3d && !name.eq_ignore_ascii_case("2d") {
        parts.push(name.to_owned());
    }
    parts.extend(resolution(version.width, version.height));
    if !version.hdr.is_empty() {
        parts.push(version.hdr.clone());
    }
    parts.extend(codec_name(&version.video_codec));
    if let Some(first) = version.audio.first() {
        let mut audio = audio_stream(first);
        if version.audio.len() > 1 {
            audio.push_str(&format!(" +{}", version.audio.len() - 1));
        }
        parts.push(audio);
    }
    parts.extend(file_size(version.size_bytes));
    if version.damaged {
        parts.push("Damaged".to_owned());
    }
    if parts.is_empty() {
        return "2D".to_owned();
    }
    parts.join(" · ")
}

/// The rows a title page lists for the chosen version, as (label, value).
/// The file row appears only when the server sent a path, which it does for
/// admins alone.
pub fn version_facts(version: &Version) -> Vec<(&'static str, String)> {
    let mut rows = Vec::new();
    let video: Vec<String> = [
        resolution(version.width, version.height),
        (!version.hdr.is_empty()).then(|| version.hdr.clone()),
        codec_name(&version.video_codec),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !video.is_empty() {
        rows.push(("Video", video.join(" · ")));
    }
    if !version.audio.is_empty() {
        let tracks: Vec<String> = version
            .audio
            .iter()
            .map(|a| match language_name(&a.language) {
                Some(lang) => format!("{lang} {}", audio_stream(a)),
                None => audio_stream(a),
            })
            .collect();
        rows.push(("Audio", tracks.join(", ")));
    }
    let file: Vec<String> = [
        (!version.container.is_empty()).then(|| version.container.to_ascii_uppercase()),
        file_size(version.size_bytes),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !file.is_empty() {
        rows.push(("File", file.join(" · ")));
    }
    if !version.path.is_empty() {
        rows.push(("Path", version.path.clone()));
    }
    rows
}

/// The version to play: the stored pick when the title still has it, else
/// the primary (`""`).
pub fn chosen_version<'a>(versions: &'a [Version], stored: Option<&str>) -> &'a str {
    stored
        .and_then(|s| versions.iter().find(|v| v.id == s))
        .map_or("", |v| v.id.as_str())
}

/// The 3D packing of the version playing: `version` as [`chosen_version`]
/// names it (`""` is the primary).
pub fn playing_stereo(versions: &[Version], version: &str) -> Option<Stereo> {
    versions
        .iter()
        .find(|v| {
            if version.is_empty() {
                v.primary
            } else {
                v.id == version
            }
        })
        .and_then(|v| v.stereo)
}

/// Where the picture sits in the playing version's frame when the file has
/// black bars baked into it (B1017), as fractions of the frame.
pub fn playing_picture(versions: &[Version], version: &str) -> Option<FrameRect> {
    versions
        .iter()
        .find(|v| {
            if version.is_empty() {
                v.primary
            } else {
                v.id == version
            }
        })
        .and_then(|v| super::bars::picture_rect(v.picture, v.width, v.height))
}

/// How far a version's runtime may stray from the primary's and still be
/// taken as the same cut: a container's rounding and a trailing black frame
/// stay inside it (spec 034 US4, V904).
pub const RUNTIME_TOLERANCE_MS: u64 = 2_000;

/// How much longer (positive) or shorter `chosen` runs than the primary, when
/// both runtimes are known and differ by more than [`RUNTIME_TOLERANCE_MS`].
pub fn runtime_gap_ms(versions: &[Version], chosen: &str) -> Option<i64> {
    let primary = versions.iter().find(|v| v.primary)?;
    let version = versions.iter().find(|v| v.id == chosen)?;
    if primary.duration_ms == 0 || version.duration_ms == 0 {
        return None;
    }
    let gap = i64::try_from(version.duration_ms).ok()? - i64::try_from(primary.duration_ms).ok()?;
    (gap.unsigned_abs() > RUNTIME_TOLERANCE_MS).then_some(gap)
}

/// What a member in a group is told about a version that runs `gap_ms`
/// longer (or shorter) than the primary.
pub fn runtime_gap_notice(gap_ms: i64) -> String {
    let secs = gap_ms.unsigned_abs().div_ceil(1_000);
    let amount = if secs < 120 {
        format!("{secs} s")
    } else {
        format!("{} min", secs.div_ceil(60))
    };
    let way = if gap_ms > 0 { "longer" } else { "shorter" };
    format!("This version runs {amount} {way} than the main one, so it may be out of step with the group.")
}

/// `4K`, `1080p`, or the height in lines; `None` when unknown. A scope or
/// cropped frame is named by its width, so a 1920×800 film still reads 1080p.
fn resolution(width: u32, height: u32) -> Option<String> {
    let label = match (width, height) {
        (0, 0) => return None,
        (w, h) if w >= 3200 || h >= 2000 => "4K".to_owned(),
        (w, h) if w >= 1800 || h >= 1000 => "1080p".to_owned(),
        (w, h) if w >= 1200 || h >= 700 => "720p".to_owned(),
        (_, h) if h > 0 => format!("{h}p"),
        _ => return None,
    };
    Some(label)
}

fn codec_name(codec: &str) -> Option<String> {
    let name = match codec.to_ascii_lowercase().as_str() {
        "" => return None,
        "h264" | "avc" | "avc1" => "H.264",
        "hevc" | "h265" | "hvc1" => "HEVC",
        "av1" => "AV1",
        "vp9" => "VP9",
        "mpeg4" | "xvid" | "divx" => "MPEG-4",
        "mpeg2video" => "MPEG-2",
        "vc1" => "VC-1",
        other => return Some(other.to_ascii_uppercase()),
    };
    Some(name.to_owned())
}

/// `EAC3 5.1`, `AAC 2.0`, `DTS`: the codec and its channel layout.
fn audio_stream(audio: &VersionAudio) -> String {
    let codec = audio.codec.to_ascii_uppercase();
    let layout = match audio.channels {
        0 => None,
        1 => Some("1.0".to_owned()),
        2 => Some("2.0".to_owned()),
        n => Some(format!("{}.1", n - 1)),
    };
    match (codec.is_empty(), layout) {
        (false, Some(l)) => format!("{codec} {l}"),
        (false, None) => codec,
        (true, Some(l)) => l,
        (true, None) => "Audio".to_owned(),
    }
}

/// An ISO 639 code as a reader knows it; an unknown code is shown as given,
/// and `und` (undetermined) or empty as nothing.
fn language_name(code: &str) -> Option<String> {
    let name = match code.to_ascii_lowercase().as_str() {
        "" | "und" => return None,
        "en" | "eng" => "English",
        "ja" | "jpn" => "Japanese",
        "fr" | "fre" | "fra" => "French",
        "de" | "ger" | "deu" => "German",
        "es" | "spa" => "Spanish",
        "it" | "ita" => "Italian",
        "ko" | "kor" => "Korean",
        "zh" | "chi" | "zho" => "Chinese",
        other => return Some(other.to_owned()),
    };
    Some(name.to_owned())
}

/// `4.2 GB`, `700 MB`; `None` when unknown.
fn file_size(bytes: u64) -> Option<String> {
    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000_000_000;
    match bytes {
        0 => None,
        b if b >= GB => Some(format!("{:.1} GB", b as f64 / GB as f64)),
        b => Some(format!("{} MB", b.div_ceil(MB))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    fn timed(id: &str, duration_ms: u64) -> Version {
        Version {
            id: id.into(),
            primary: id.is_empty(),
            duration_ms,
            ..Default::default()
        }
    }

    /// Spec 034 US4 (V904).
    #[test]
    fn a_version_that_runs_differently_is_told_apart_from_rounding() {
        let versions = [
            timed("", 8_400_000),
            timed("3d", 8_440_000),
            timed("rip", 8_401_500),
        ];
        assert_eq!(runtime_gap_ms(&versions, "3d"), Some(40_000));
        assert_eq!(
            runtime_gap_ms(&versions, "rip"),
            None,
            "inside the tolerance"
        );
        assert_eq!(runtime_gap_ms(&versions, ""), None, "the primary itself");
        let unknown = [timed("", 8_400_000), timed("3d", 0)];
        assert_eq!(runtime_gap_ms(&unknown, "3d"), None);
        assert_eq!(
            runtime_gap_notice(40_000),
            "This version runs 40 s longer than the main one, so it may be out of step with the group."
        );
        assert!(runtime_gap_notice(-300_000).contains("5 min shorter"));
    }

    fn version(id: &str, name: &str, stereo: Option<Stereo>) -> Version {
        Version {
            id: id.into(),
            name: name.into(),
            stereo,
            primary: id.is_empty(),
            ..Default::default()
        }
    }

    #[test]
    fn the_playing_picture_is_the_picked_versions_bars() {
        let bars = Version {
            width: 1920,
            height: 1080,
            picture: Some(pharos_client::api::PictureArea {
                x: 240,
                y: 0,
                width: 1440,
                height: 1080,
            }),
            ..version("", "", None)
        };
        let clean = Version {
            width: 1920,
            height: 1080,
            ..version("remaster", "Remaster", None)
        };
        let versions = [bars, clean];
        let r = playing_picture(&versions, "").expect("the primary has bars");
        assert_eq!((r.x, r.w), (0.125, 0.75));
        assert_eq!(playing_picture(&versions, "remaster"), None);
        assert_eq!(playing_picture(&versions, "gone"), None);
    }

    #[test]
    fn the_playing_packing_is_the_picked_versions_or_the_primarys() {
        let versions = [
            version("", "", None),
            version("3d-hsbs", "3D", Some(Stereo::HalfSideBySide)),
        ];
        assert_eq!(playing_stereo(&versions, ""), None);
        assert_eq!(
            playing_stereo(&versions, "3d-hsbs"),
            Some(Stereo::HalfSideBySide)
        );
        assert_eq!(playing_stereo(&versions, "gone"), None);
        let lone = [version("", "", Some(Stereo::FullTopAndBottom))];
        assert_eq!(playing_stereo(&lone, ""), Some(Stereo::FullTopAndBottom));
    }

    #[test]
    fn labels_say_2d_or_3d_and_the_packing() {
        assert_eq!(version_label(&version("", "", None)), "2D");
        assert_eq!(
            version_label(&version("3d-hsbs", "3D", Some(Stereo::HalfSideBySide))),
            "3D (half side-by-side)"
        );
        assert_eq!(
            version_label(&version("3d-hsbs", "3D HSBS", Some(Stereo::HalfSideBySide))),
            "3D (half side-by-side)"
        );
        assert_eq!(
            version_label(&version("x", "Extended", Some(Stereo::FullTopAndBottom))),
            "3D (full top-and-bottom) · Extended"
        );
        assert_eq!(
            version_label(&version("m", "", Some(Stereo::Mvc))),
            "3D (MVC, plays in 2D here)"
        );
    }

    fn copy() -> Version {
        Version {
            id: "season-1".into(),
            name: "Season 1".into(),
            width: 3840,
            height: 2160,
            video_codec: "hevc".into(),
            hdr: "HDR10".into(),
            audio: vec![
                VersionAudio {
                    language: "eng".into(),
                    codec: "eac3".into(),
                    channels: 6,
                },
                VersionAudio {
                    language: "jpn".into(),
                    codec: "aac".into(),
                    channels: 2,
                },
            ],
            size_bytes: 4_200_000_000,
            container: "mkv".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_copy_is_labelled_by_its_file() {
        assert_eq!(
            version_label(&copy()),
            "Season 1 · 4K · HDR10 · HEVC · EAC3 5.1 +1 · 4.2 GB"
        );
        let scope = Version {
            width: 1920,
            height: 800,
            video_codec: "h264".into(),
            ..Default::default()
        };
        assert_eq!(version_label(&scope), "1080p · H.264");
        let small = Version {
            size_bytes: 700_000_000,
            ..Default::default()
        };
        assert_eq!(version_label(&small), "700 MB");
    }

    #[test]
    fn facts_list_every_track_and_the_path_only_when_sent() {
        assert_eq!(
            version_facts(&copy()),
            [
                ("Video", "4K · HDR10 · HEVC".to_owned()),
                ("Audio", "English EAC3 5.1, Japanese AAC 2.0".to_owned()),
                ("File", "MKV · 4.2 GB".to_owned()),
            ]
        );
        let admin = Version {
            path: "/media/TV/Show/Season 1/Show - S01E02.mkv".into(),
            ..copy()
        };
        assert_eq!(
            version_facts(&admin).last(),
            Some(&(
                "Path",
                "/media/TV/Show/Season 1/Show - S01E02.mkv".to_owned()
            ))
        );
        assert!(version_facts(&Version::default()).is_empty());
    }

    #[test]
    fn the_stored_pick_wins_while_the_title_has_it() {
        let versions = [
            version("", "", None),
            version("3d-hsbs", "", Some(Stereo::HalfSideBySide)),
        ];
        assert_eq!(chosen_version(&versions, Some("3d-hsbs")), "3d-hsbs");
        assert_eq!(chosen_version(&versions, Some("")), "");
        assert_eq!(chosen_version(&versions, Some("gone")), "");
    }

    #[test]
    fn nothing_stored_plays_the_primary_even_with_a_3d_version() {
        let versions = [
            version("", "", None),
            version("3d-hsbs", "", Some(Stereo::HalfSideBySide)),
        ];
        assert_eq!(chosen_version(&versions, None), "");
    }
}
