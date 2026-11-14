//! An audiobook's own behaviour in the native UI (ADR-0132, spec 046).

use pharos_client::api::{Item, ItemKind};

/// The track an audiobook's Play starts on: the last one begun and not
/// finished, else the first not finished, else the first (V1403).
pub fn resume_track(tracks: &[Item]) -> Option<&Item> {
    let songs = || tracks.iter().filter(|t| t.kind == ItemKind::Audio);
    songs()
        .rfind(|t| t.position_ms > 0 && !t.played)
        .or_else(|| songs().find(|t| !t.played))
        .or_else(|| songs().next())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use pharos_client::api::ItemRef;

    fn track(n: u64, position_ms: u64, played: bool) -> Item {
        Item {
            id: ItemRef::Real(n),
            kind: ItemKind::Audio,
            name: format!("Part {n}"),
            year: None,
            runtime_ms: 600_000,
            image: None,
            played,
            favorite: false,
            position_ms,
            series_name: None,
            season: None,
            episode: Some(u32::try_from(n).unwrap()),
            parent: None,
            library_kind: None,
            imdb_rating: None,
            critic_rating: None,
            artist: None,
            stereo: None,
            damaged: false,
            new_children: 0,
            audiobook: true,
        }
    }

    #[test]
    fn play_resumes_the_track_last_begun() {
        let fresh = [track(1, 0, false), track(2, 0, false)];
        assert_eq!(resume_track(&fresh).unwrap().id, ItemRef::Real(1));
        let begun = [
            track(1, 0, true),
            track(2, 90_000, false),
            track(3, 0, false),
        ];
        assert_eq!(resume_track(&begun).unwrap().id, ItemRef::Real(2));
        let between = [track(1, 0, true), track(2, 0, true), track(3, 0, false)];
        assert_eq!(resume_track(&between).unwrap().id, ItemRef::Real(3));
        let done = [track(1, 0, true), track(2, 0, true)];
        assert_eq!(resume_track(&done).unwrap().id, ItemRef::Real(1));
        assert!(resume_track(&[]).is_none());
    }
}
