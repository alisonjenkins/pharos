//! A fetched text subtitle standing in for a disc-picture track (V1509).
//!
//! When a language's only embedded tracks are pictures (PGS, VobSub, DVB),
//! the background job fetches a text version (V1508). That text track takes
//! the picture track's place in the automatic pick: it is chosen exactly when
//! the picture track would have been, and never otherwise, so fetching it
//! cannot switch subtitles on for anyone. The picture track stays listed as
//! the fallback a viewer can still pick by hand.

use super::job::same_language;
use super::store;
use crate::api::jellyfin::stream_select::StreamFacts;
use crate::api::jellyfin::subtitles::{
    is_image_subtitle_codec, sidecar_base_index, sidecar_is_sdh, sidecar_language_from_name,
    SidecarKind,
};
use pharos_core::{MediaProbe, SubtitleTrack};
use std::path::{Path, PathBuf};

/// One picture track and the fetched text track that replaces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StandIn {
    pub picture_index: u32,
    pub text_index: u32,
    pub picture_is_default: bool,
    /// `false` for another fetched file in the picture track's language: it
    /// is held out of the automatic ranking like the paired one, but never
    /// started in the picture track's place.
    pub paired: bool,
}

/// The stand-ins for a probe, from the same sidecar scan that built its
/// advertised streams, so each text index names the file the streams list
/// under it. A second scan could see a download that sorts before an
/// existing sidecar and shift every index after it.
pub fn from_sidecars(probe: &MediaProbe, sidecars: &[(PathBuf, SidecarKind)]) -> Vec<StandIn> {
    let Some(root) = store::installed_root() else {
        return Vec::new();
    };
    pair(probe, root, sidecars.iter().map(|(p, _)| p.as_path()))
}

fn is_picture(t: &SubtitleTrack) -> bool {
    t.codec.as_deref().is_some_and(is_image_subtitle_codec)
}

/// Pair each picture track of a picture-only language with a fetched text
/// sidecar in that language, an SDH one for an SDH picture track when there
/// is one. `sidecars` is in discovery order, which is the stream index order.
fn pair<'a>(
    probe: &MediaProbe,
    root: &Path,
    sidecars: impl Iterator<Item = &'a Path>,
) -> Vec<StandIn> {
    let base = sidecar_base_index(probe);
    let fetched: Vec<(u32, String, bool)> = sidecars
        .enumerate()
        .filter(|(_, p)| p.starts_with(root))
        .filter_map(|(offset, p)| {
            let name = p.file_name()?.to_str()?;
            let language = sidecar_language_from_name(name)?;
            let index = base.checked_add(u32::try_from(offset).ok()?)?;
            Some((index, language, sidecar_is_sdh(name)))
        })
        .collect();
    let in_language = |t: &SubtitleTrack, language: &str| {
        !t.is_forced
            && t.language
                .as_deref()
                .is_some_and(|l| same_language(l, language))
    };
    let mut out = Vec::new();
    for picture in probe
        .subtitle_tracks
        .iter()
        .filter(|t| !t.is_forced && is_picture(t))
    {
        let Some(language) = picture.language.as_deref() else {
            continue;
        };
        let has_text = probe
            .subtitle_tracks
            .iter()
            .any(|t| in_language(t, language) && !is_picture(t));
        if has_text {
            continue;
        }
        let same = |(_, l, _): &&(u32, String, bool)| same_language(l, language);
        let text = fetched
            .iter()
            .filter(same)
            .find(|(_, _, sdh)| *sdh == picture.is_hearing_impaired)
            .or_else(|| fetched.iter().find(same));
        if let Some((text_index, _, _)) = text {
            out.push(StandIn {
                picture_index: picture.stream_index,
                text_index: *text_index,
                picture_is_default: picture.is_default,
                paired: true,
            });
        }
    }
    // Every other fetched file in a stand-in's language is held out of the
    // ranking too: ranked as an external track it could win over the picture
    // track it stands beside.
    let paired: Vec<(u32, u32, bool)> = out
        .iter()
        .map(|s| (s.picture_index, s.text_index, s.picture_is_default))
        .collect();
    for (picture_index, text_index, picture_is_default) in paired {
        let Some(language) = probe
            .subtitle_tracks
            .iter()
            .find(|t| t.stream_index == picture_index)
            .and_then(|t| t.language.as_deref())
        else {
            continue;
        };
        for (index, l, _) in &fetched {
            if *index != text_index
                && same_language(l, language)
                && !out.iter().any(|s| s.text_index == *index)
            {
                out.push(StandIn {
                    picture_index,
                    text_index: *index,
                    picture_is_default,
                    paired: false,
                });
            }
        }
    }
    out
}

/// Leave the stand-ins, and every other fetched file in their languages, out
/// of the ranking, so only their picture tracks are ranked, with the picture track's own language, default and forced flags.
/// Ranked as an external track, or under the sidecar's own language tag, a
/// stand-in could be selected where its picture track would not be.
pub fn adjust_facts(facts: &mut Vec<StreamFacts>, stand_ins: &[StandIn]) {
    facts.retain(|f| !stand_ins.iter().any(|s| s.text_index == f.index));
}

/// The text track to start on in place of `chosen`, when `chosen` is a
/// picture track with a stand-in.
pub fn replacement(chosen: u32, stand_ins: &[StandIn]) -> Option<u32> {
    stand_ins
        .iter()
        .find(|s| s.paired && s.picture_index == chosen)
        .map(|s| s.text_index)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::path::PathBuf;

    fn track(index: u32, codec: &str, sdh: bool, default: bool) -> SubtitleTrack {
        SubtitleTrack {
            stream_index: index,
            language: Some("eng".into()),
            codec: Some(codec.into()),
            is_hearing_impaired: sdh,
            is_default: default,
            ..Default::default()
        }
    }

    fn probe(tracks: Vec<SubtitleTrack>) -> MediaProbe {
        MediaProbe {
            subtitle_tracks: tracks,
            ..MediaProbe::default()
        }
    }

    fn paths(root: &Path, names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(|n| root.join("00ab").join(n)).collect()
    }

    #[test]
    fn a_fetched_text_track_replaces_each_picture_track_sdh_matched() {
        let root = Path::new("/cache/opensubtitles");
        let p = probe(vec![
            track(2, "hdmv_pgs_subtitle", false, true),
            track(3, "hdmv_pgs_subtitle", true, false),
        ]);
        let base = sidecar_base_index(&p);
        let sc = paths(root, &["os1.en.srt", "os2.en.sdh.srt"]);
        let got = pair(&p, root, sc.iter().map(PathBuf::as_path));
        assert_eq!(
            got,
            vec![
                StandIn {
                    picture_index: 2,
                    text_index: base,
                    picture_is_default: true,
                    paired: true,
                },
                StandIn {
                    picture_index: 3,
                    text_index: base + 1,
                    picture_is_default: false,
                    paired: true,
                },
            ]
        );
    }

    /// A second fetched file in the language is held out of the ranking, so
    /// it cannot win as an external track, and is never started in the
    /// picture track's place.
    #[test]
    fn another_fetched_file_in_the_language_is_held_out_of_the_ranking() {
        let root = Path::new("/cache/opensubtitles");
        let p = probe(vec![track(2, "hdmv_pgs_subtitle", false, true)]);
        let base = sidecar_base_index(&p);
        let sc = paths(root, &["os1.en.srt", "os2.en.srt", "os3.fr.srt"]);
        let got = pair(&p, root, sc.iter().map(PathBuf::as_path));
        let mut facts: Vec<StreamFacts> = [2, base, base + 1, base + 2]
            .into_iter()
            .map(|index| StreamFacts {
                index,
                is_external: index >= base,
                ..StreamFacts::default()
            })
            .collect();
        adjust_facts(&mut facts, &got);
        assert_eq!(
            facts.iter().map(|f| f.index).collect::<Vec<_>>(),
            [2, base + 2],
            "both English files are held out; French is not a stand-in language"
        );
        assert_eq!(replacement(2, &got), Some(base));
        assert_eq!(got.iter().filter(|s| s.paired).count(), 1);
    }

    #[test]
    fn no_stand_in_beside_a_text_track_or_for_a_sidecar_outside_the_store() {
        let root = Path::new("/cache/opensubtitles");
        let with_text = probe(vec![
            track(2, "hdmv_pgs_subtitle", false, false),
            track(3, "subrip", false, false),
        ]);
        let sc = paths(root, &["os1.en.srt"]);
        assert!(pair(&with_text, root, sc.iter().map(PathBuf::as_path)).is_empty());
        let pictures = probe(vec![track(2, "dvd_subtitle", false, false)]);
        let own = [PathBuf::from("/media/film.en.srt")];
        assert!(pair(&pictures, root, own.iter().map(PathBuf::as_path)).is_empty());
    }

    /// The text index is the file's place in the scan the streams were built
    /// from, wherever a fetched file sorts among the other sidecars.
    #[test]
    fn a_stand_in_names_its_place_in_the_given_scan() {
        let root = Path::new("/cache/opensubtitles");
        let p = probe(vec![track(2, "hdmv_pgs_subtitle", false, true)]);
        let base = sidecar_base_index(&p);
        let scan = [
            root.join("00ab").join("os1.en.srt"),
            PathBuf::from("/media/film.fr.srt"),
        ];
        let got = pair(&p, root, scan.iter().map(PathBuf::as_path));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text_index, base);
        let scan = [
            PathBuf::from("/media/film.fr.srt"),
            root.join("00ab").join("os1.en.srt"),
        ];
        let got = pair(&p, root, scan.iter().map(PathBuf::as_path));
        assert_eq!(got[0].text_index, base + 1);
    }

    #[test]
    fn a_stand_in_ranks_as_its_picture_track_and_is_what_starts() {
        let s = [StandIn {
            picture_index: 2,
            text_index: 9,
            picture_is_default: true,
            paired: true,
        }];
        let mut facts = vec![
            StreamFacts {
                index: 2,
                is_default: true,
                ..StreamFacts::default()
            },
            StreamFacts {
                index: 9,
                is_external: true,
                ..StreamFacts::default()
            },
        ];
        adjust_facts(&mut facts, &s);
        assert_eq!(
            facts.iter().map(|f| f.index).collect::<Vec<_>>(),
            [2],
            "only the picture track is ranked"
        );
        assert_eq!(replacement(2, &s), Some(9));
        assert_eq!(replacement(9, &s), None);
        assert_eq!(replacement(4, &s), None);
    }
}
