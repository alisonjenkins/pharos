//! Media segments (Skip Intro / Skip Outro): an item's chapter-derived and
//! detected spans, served to Jellyfin clients as `/MediaSegments/{id}` and to
//! the native player on its item detail (T86, ADR-0018 #6).

use std::collections::HashSet;

use pharos_core::{MediaItem, MediaSegmentStore as _};

use crate::state::AppState;

/// How long a chapter segment runs when the probe gave it no end.
const CHAPTER_FALLBACK_MS: u64 = 30_000;

/// One skippable span of an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// Stable within the item: the chapter's position, or `d{n}` for the n-th
    /// detected segment. Jellyfin segment ids are derived from it.
    pub key: String,
    /// The Jellyfin `MediaSegmentType`: `Intro`, `Outro`, `Recap`, `Preview`
    /// or `Commercial`.
    pub kind: &'static str,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// The item's segments: labelled chapters first, then the detected segments
/// whose type no chapter already supplied.
pub async fn for_item(state: &AppState, item: &MediaItem) -> Vec<Segment> {
    let mut out = Vec::new();
    // Track which segment Types a CHAPTER already supplied — chapters are exact
    // (author-labeled) so they win over a fingerprint/black-frame detection for
    // the same Type (T86/ADR-0018 #6, layered sources).
    let mut seen_types: HashSet<&'static str> = HashSet::new();
    for (idx, c) in item.probe.chapters.iter().enumerate() {
        let Some(kind) = classify_chapter_title(&c.title) else {
            continue;
        };
        // A second chapter of the same Type is a boundary marker ("Intro
        // end"), not a real second span — ripper tools title the chapter
        // AFTER the intro to describe the transition, and it matches the
        // same substring. Keep only the first (B201/V154).
        if !seen_types.insert(kind) {
            continue;
        }
        // The chapter's `end_ms` is the *next* chapter's start —
        // ffprobe carries it explicitly so we trust it.
        let end_ms = if c.end_ms > c.start_ms {
            c.end_ms
        } else {
            c.start_ms.saturating_add(CHAPTER_FALLBACK_MS)
        };
        out.push(Segment {
            key: idx.to_string(),
            kind,
            start_ms: c.start_ms,
            end_ms,
        });
    }
    // T86 — union the auto-DETECTED segments (audio-fingerprint intro/outro,
    // black-frame credits) the backfill persisted, skipping any Type a chapter
    // already covered. This is what makes Skip Intro / Skip Outro work for the
    // vast majority of TV rips, which carry no labeled intro chapters.
    let detected = match state.stores.media_segments_for(item.id).await {
        Ok(detected) => detected,
        Err(e) => {
            tracing::warn!(
                media.id = item.id,
                error = %e,
                "media segments: detected segments unreadable, serving chapters only"
            );
            Vec::new()
        }
    };
    for (di, seg) in detected.iter().enumerate() {
        let kind = seg.kind.as_str();
        if seen_types.contains(kind) {
            continue;
        }
        out.push(Segment {
            key: format!("d{di}"),
            kind,
            start_ms: seg.start_ms,
            end_ms: seg.end_ms,
        });
    }
    out
}

/// Whether this item has anything for a client to skip — a detected segment or
/// a chapter whose title classifies as one.
///
/// B130 — this is what `MediaSource.HasSegments` must carry.
/// jellyfin-web's `MediaSegmentManager.onPlayerPlaybackStart` reads that field
/// and returns BEFORE calling `getItemSegments` when it is false, so a `false`
/// here makes `/MediaSegments/{id}` unreachable from the web client no matter
/// how correct its answer would be.
///
/// Answered from the same two sources [`for_item`] serves, so the
/// advertisement cannot disagree with the response: if this says yes, that
/// endpoint returns at least one segment.
pub async fn any(state: &AppState, item: &MediaItem) -> bool {
    if item
        .probe
        .chapters
        .iter()
        .any(|c| classify_chapter_title(&c.title).is_some())
    {
        return true;
    }
    matches!(state.stores.media_segments_for(item.id).await, Ok(s) if !s.is_empty())
}

/// Map a free-text chapter title to a Jellyfin MediaSegmentType.
/// Returns None when the title doesn't look like an actionable
/// segment.
pub fn classify_chapter_title(title: &str) -> Option<&'static str> {
    let lc = title.to_ascii_lowercase();
    let song = theme_song(&lc);
    if lc.contains("intro") || lc.contains("opening") || song == Some("op") {
        return Some("Intro");
    }
    if lc.contains("outro")
        || lc.contains("credits")
        || lc.contains("closing")
        || matches!(song, Some("ed" | "ending"))
    {
        return Some("Outro");
    }
    if lc.contains("recap") || lc.contains("previously on") {
        return Some("Recap");
    }
    if lc.contains("preview") || lc.contains("next on") {
        return Some("Preview");
    }
    if lc.contains("commercial") {
        return Some("Commercial");
    }
    None
}

/// The theme-song word of an anime rip's chapter title ("OP", "[ED 2]",
/// "Ending Theme"): its first word, when the rest is only a number or
/// "theme"/"song". `None` for any other title, so "The Ending" and "Ed meets
/// the king" stay ordinary chapters.
fn theme_song(lc: &str) -> Option<&str> {
    let mut words = lc
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty());
    let first = words.next()?.trim_end_matches(|c: char| c.is_ascii_digit());
    words
        .all(|w| w.bytes().all(|b| b.is_ascii_digit()) || w == "theme" || w == "song")
        .then_some(first)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn classify_chapter_title_branches() {
        assert_eq!(classify_chapter_title("Opening"), Some("Intro"));
        assert_eq!(classify_chapter_title("Intro 1"), Some("Intro"));
        assert_eq!(classify_chapter_title("End credits"), Some("Outro"));
        assert_eq!(classify_chapter_title("Credits"), Some("Outro"));
        assert_eq!(classify_chapter_title("Rolling credits"), Some("Outro"));
        assert_eq!(classify_chapter_title("Opening Credits"), Some("Intro"));
        assert_eq!(classify_chapter_title("OP"), Some("Intro"));
        assert_eq!(classify_chapter_title("ED"), Some("Outro"));
        assert_eq!(classify_chapter_title("ED 2"), Some("Outro"));
        assert_eq!(classify_chapter_title("Ending"), Some("Outro"));
        assert_eq!(classify_chapter_title("Ending Theme"), Some("Outro"));
        assert_eq!(classify_chapter_title("[ED]"), Some("Outro"));
        assert_eq!(classify_chapter_title("OP1"), Some("Intro"));
        assert_eq!(classify_chapter_title("The Ending"), None);
        assert_eq!(classify_chapter_title("Ed meets the king"), None);
        assert_eq!(classify_chapter_title("Ending the feud"), None);
        assert_eq!(classify_chapter_title("Pending"), None);
        assert_eq!(classify_chapter_title("Edward"), None);
        assert_eq!(classify_chapter_title("Operation"), None);
        assert_eq!(classify_chapter_title("Previously on"), Some("Recap"));
        assert_eq!(classify_chapter_title("Recap"), Some("Recap"));
        assert_eq!(classify_chapter_title("Next on"), Some("Preview"));
        assert_eq!(
            classify_chapter_title("Commercial break"),
            Some("Commercial")
        );
        assert_eq!(classify_chapter_title("Chapter 4"), None);
        assert_eq!(classify_chapter_title("The Beach"), None);
    }

    #[actix_web::test]
    async fn keeps_only_the_first_chapter_per_type() {
        // B201: "Intro start" / "Intro end" is a boundary-marker pair, not
        // two real intro spans. The second chapter must not surface as its
        // own segment covering the rest of the episode (V154).
        use crate::state::{AppState, Stores};
        use pharos_core::{MediaChapter, MediaKind, MediaProbe};

        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        let item = MediaItem {
            id: 42,
            path: "/sabrina-s01e02.mkv".into(),
            title: "Chapter Two".into(),
            kind: MediaKind::Episode,
            probe: MediaProbe {
                chapters: vec![
                    MediaChapter {
                        start_ms: 0,
                        end_ms: 102_853,
                        title: "Intro start".into(),
                    },
                    MediaChapter {
                        start_ms: 102_853,
                        end_ms: 3_429_760,
                        title: "Intro end".into(),
                    },
                ],
                ..Default::default()
            },
            ..Default::default()
        };
        let state = AppState::new(stores, "t".into());

        let segs = for_item(&state, &item).await;
        let intros: Vec<_> = segs.iter().filter(|s| s.kind == "Intro").collect();
        assert_eq!(
            intros.len(),
            1,
            "a same-Type second chapter must be dropped as a boundary marker, not unioned in: {segs:?}"
        );
    }
}
