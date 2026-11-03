//! `.lrc` and `.txt` files beside a song, sharing its file stem.

use std::path::Path;

use super::{lrc, Lyrics, Origin};

/// The `.lrc` sidecar. Usually synced; plain text in an `.lrc` still counts.
pub fn lrc(media: &Path) -> Option<Lyrics> {
    read(media, "lrc", Origin::SidecarLrc)
}

/// The `.txt` sidecar, parsed the same way, so a timed `.txt` still syncs.
pub fn text(media: &Path) -> Option<Lyrics> {
    read(media, "txt", Origin::SidecarText)
}

fn read(media: &Path, extension: &str, origin: Origin) -> Option<Lyrics> {
    let path = media.with_extension(extension);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "lyrics: sidecar unreadable");
            return None;
        }
    };
    // Old sidecars are often Latin-1; a lossy read shows most of the words
    // rather than none.
    let text = String::from_utf8_lossy(&bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    Lyrics::from_lines(lrc::parse(text), origin)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn reads_the_sidecar_with_the_songs_stem_and_drops_a_bom() {
        let dir = tempfile::tempdir().unwrap();
        let song = dir.path().join("01 Song.flac");
        std::fs::write(dir.path().join("01 Song.lrc"), "\u{feff}[00:02.00]Hi").unwrap();
        let found = lrc(&song).unwrap();
        assert_eq!(found.origin, Origin::SidecarLrc);
        assert!(found.synced());
        assert_eq!(found.lines.first().unwrap().text, "Hi");
        assert_eq!(text(&song), None);
    }

    #[test]
    fn a_text_sidecar_is_plain_lyrics() {
        let dir = tempfile::tempdir().unwrap();
        let song = dir.path().join("song.mp3");
        std::fs::write(dir.path().join("song.txt"), "la la\nla").unwrap();
        let found = text(&song).unwrap();
        assert_eq!(found.origin, Origin::SidecarText);
        assert!(!found.synced());
        assert_eq!(found.lines.len(), 2);
    }

    #[test]
    fn an_empty_sidecar_is_no_lyrics() {
        let dir = tempfile::tempdir().unwrap();
        let song = dir.path().join("song.mp3");
        std::fs::write(dir.path().join("song.lrc"), "[ar:Band]\n").unwrap();
        assert_eq!(lrc(&song), None);
    }
}
