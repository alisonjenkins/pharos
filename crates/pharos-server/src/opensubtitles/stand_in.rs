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
    discover_sidecars, is_image_subtitle_codec, sidecar_base_index, sidecar_is_sdh,
    sidecar_language_from_name,
};
use pharos_core::{MediaItem, MediaProbe, SubtitleTrack};
use std::path::Path;

/// One picture track and the fetched text track that replaces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StandIn {
    pub picture_index: u32,
    pub text_index: u32,
    pub picture_is_default: bool,
}

/// The stand-ins for `item`, from its sidecars in the OpenSubtitles store.
pub async fn for_item(item: &MediaItem) -> Vec<StandIn> {
    let Some(root) = store::installed_root() else {
        return Vec::new();
    };
    let sidecars = discover_sidecars(&item.path).await;
    let fetched = sidecars.iter().map(|(p, _)| p.as_path());
    pair(&item.probe, root, fetched)
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
            });
        }
    }
    out
}

/// Give each stand-in the place of the picture track it replaces: not ranked
/// as an external track (which would select it where the picture track would
/// not be), and default when its picture track is.
pub fn adjust_facts(facts: &mut [StreamFacts], stand_ins: &[StandIn]) {
    for f in facts.iter_mut() {
        let mut pictures = stand_ins
            .iter()
            .filter(|s| s.text_index == f.index)
            .peekable();
        if pictures.peek().is_none() {
            continue;
        }
        f.is_external = false;
        f.is_default = pictures.any(|s| s.picture_is_default);
    }
}

/// The text track to start on in place of `chosen`, when `chosen` is a
/// picture track with a stand-in, or is a stand-in itself.
pub fn replacement(chosen: u32, stand_ins: &[StandIn]) -> Option<u32> {
    stand_ins
        .iter()
        .find(|s| s.picture_index == chosen || s.text_index == chosen)
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
                    picture_is_default: true
                },
                StandIn {
                    picture_index: 3,
                    text_index: base + 1,
                    picture_is_default: false
                },
            ]
        );
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

    #[test]
    fn a_stand_in_ranks_as_its_picture_track_and_is_what_starts() {
        let s = [StandIn {
            picture_index: 2,
            text_index: 9,
            picture_is_default: true,
        }];
        let mut facts = vec![StreamFacts {
            index: 9,
            is_external: true,
            ..StreamFacts::default()
        }];
        adjust_facts(&mut facts, &s);
        assert!(!facts[0].is_external && facts[0].is_default);
        assert_eq!(replacement(2, &s), Some(9));
        assert_eq!(replacement(9, &s), Some(9));
        assert_eq!(replacement(4, &s), None);
    }
}
