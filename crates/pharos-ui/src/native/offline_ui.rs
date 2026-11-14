//! Offline music on the phone (spec 044 T24-T26): the sums and wording the
//! settings section, the keep toggle and the Downloads view share. They read
//! the [`OfflineStore`](super::ports::OfflineStore) port, which is registered
//! only in the phone app, so none of it shows anywhere else.

use super::ports::{ContainerRef, Download, TrackState};

pub const GIB: u64 = 1024 * 1024 * 1024;

/// The cap choices, in GiB.
pub const CAP_CHOICES_GIB: [u64; 6] = [1, 2, 4, 8, 16, 32];

/// Downloads stop this far short of filling the phone. The store enforces
/// it; the UI only says so.
pub const FREE_GUARD_BYTES: u64 = 2 * GIB;

/// A size in binary units with at most one decimal: `512 B`, `1.5 MiB`, `4 GiB`.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut unit = 0usize;
    let mut size = 1u128;
    while unit + 1 < UNITS.len() && u128::from(bytes) >= size.saturating_mul(1024) {
        size = size.saturating_mul(1024);
        unit += 1;
    }
    let name = UNITS.get(unit).copied().unwrap_or("B");
    // Rounded to a tenth in integers, so no float casts.
    let tenths = u128::from(bytes)
        .saturating_mul(10)
        .saturating_add(size / 2)
        / size;
    let (whole, frac) = (tenths / 10, tenths % 10);
    if frac == 0 {
        format!("{whole} {name}")
    } else {
        format!("{whole}.{frac} {name}")
    }
}

/// The cap choices as `Choices` options: the GiB count and its label.
pub fn cap_options() -> Vec<(String, String)> {
    CAP_CHOICES_GIB
        .iter()
        .map(|g| (g.to_string(), format!("{g} GiB")))
        .collect()
}

/// The option a cap selects, or an empty string for a cap that is not one of
/// the choices (nothing is then pressed).
pub fn cap_selected(cap_bytes: u64) -> String {
    CAP_CHOICES_GIB
        .iter()
        .find(|g| g.saturating_mul(GIB) == cap_bytes)
        .map(u64::to_string)
        .unwrap_or_default()
}

/// The bytes a `Choices` value stands for; `None` for anything else.
pub fn cap_from_option(value: &str) -> Option<u64> {
    let gib: u64 = value.parse().ok()?;
    CAP_CHOICES_GIB
        .contains(&gib)
        .then(|| gib.saturating_mul(GIB))
}

/// Where a container stands for offline, as the chip says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepState {
    NotKept,
    /// `done` of `total` tracks are on the phone and some still wait.
    Downloading {
        done: usize,
        total: usize,
    },
    /// Every track is on the phone.
    Kept,
    /// Some tracks could not be downloaded and none is still waiting.
    Failed {
        failed: usize,
    },
}

impl KeepState {
    pub fn of(downloads: &[Download], container: &ContainerRef) -> Self {
        let Some(d) = downloads.iter().find(|d| &d.container == container) else {
            return Self::NotKept;
        };
        let count = |s: TrackState| d.tracks.iter().filter(|t| t.state == s).count();
        let (cached, queued, failed) = (
            count(TrackState::Cached),
            count(TrackState::Queued),
            count(TrackState::Failed),
        );
        if queued > 0 {
            Self::Downloading {
                done: cached,
                total: d.tracks.len(),
            }
        } else if failed > 0 {
            Self::Failed { failed }
        } else {
            Self::Kept
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::NotKept => "Not kept".to_owned(),
            Self::Downloading { done, total } => format!("Downloading {done} of {total}"),
            Self::Kept => "Kept".to_owned(),
            Self::Failed { failed: 1 } => "1 track failed".to_owned(),
            Self::Failed { failed } => format!("{failed} tracks failed"),
        }
    }

    /// A stable word for CSS and tests, never shown.
    pub fn name(self) -> &'static str {
        match self {
            Self::NotKept => "not-kept",
            Self::Downloading { .. } => "downloading",
            Self::Kept => "kept",
            Self::Failed { .. } => "failed",
        }
    }

    /// The toggle is pressed once the container is pinned, whatever its tracks do.
    pub fn is_kept(self) -> bool {
        self != Self::NotKept
    }
}

/// A track's state as the Downloads list says it.
pub fn track_label(state: TrackState) -> &'static str {
    match state {
        TrackState::Queued => "Waiting",
        TrackState::Cached => "Saved",
        TrackState::Failed => "Failed",
    }
}

pub const OFFLINE_BANNER: &str = "You're offline. Only the music saved on this phone will play.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::ports::{ContainerKind, TrackDownload};

    fn album(id: &str) -> ContainerRef {
        ContainerRef {
            kind: ContainerKind::Album,
            id: id.into(),
        }
    }

    fn pinned(id: &str, states: &[TrackState]) -> Download {
        Download {
            container: album(id),
            tracks: states
                .iter()
                .enumerate()
                .map(|(i, s)| TrackDownload {
                    id: format!("t{i}"),
                    state: *s,
                    bytes: 100,
                })
                .collect(),
        }
    }

    #[test]
    fn sizes_use_binary_units_with_one_decimal() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5 MiB");
        assert_eq!(format_bytes(4 * GIB), "4 GiB");
        assert_eq!(format_bytes(GIB + GIB / 2), "1.5 GiB");
        assert_eq!(format_bytes(u64::MAX), "16777216 TiB");
    }

    #[test]
    fn a_size_just_under_a_unit_keeps_the_smaller_unit() {
        assert_eq!(format_bytes(1024 * 1024 - 1), "1024 KiB");
    }

    #[test]
    fn the_cap_options_are_the_six_gib_choices() {
        let options = cap_options();
        let values: Vec<_> = options.iter().map(|(v, _)| v.as_str()).collect();
        assert_eq!(values, ["1", "2", "4", "8", "16", "32"]);
        assert_eq!(options.first().map(|o| o.1.as_str()), Some("1 GiB"));
    }

    #[test]
    fn a_cap_selects_its_option_and_an_odd_cap_selects_none() {
        assert_eq!(cap_selected(4 * GIB), "4");
        assert_eq!(cap_selected(32 * GIB), "32");
        assert_eq!(cap_selected(3 * GIB), "");
        assert_eq!(cap_selected(GIB + 1), "");
    }

    #[test]
    fn an_option_maps_back_to_bytes_and_a_stray_value_to_none() {
        assert_eq!(cap_from_option("8"), Some(8 * GIB));
        assert_eq!(cap_from_option("3"), None);
        assert_eq!(cap_from_option("x"), None);
        assert_eq!(cap_from_option(""), None);
    }

    #[test]
    fn a_container_that_is_not_pinned_is_not_kept() {
        let state = KeepState::of(&[pinned("a", &[TrackState::Cached])], &album("b"));
        assert_eq!(state, KeepState::NotKept);
        assert_eq!(state.label(), "Not kept");
        assert!(!state.is_kept());
    }

    #[test]
    fn waiting_tracks_make_it_downloading_with_the_count_done() {
        let d = pinned(
            "a",
            &[TrackState::Cached, TrackState::Queued, TrackState::Queued],
        );
        let state = KeepState::of(&[d], &album("a"));
        assert_eq!(state, KeepState::Downloading { done: 1, total: 3 });
        assert_eq!(state.label(), "Downloading 1 of 3");
        assert!(state.is_kept());
    }

    #[test]
    fn all_tracks_cached_is_kept() {
        let d = pinned("a", &[TrackState::Cached, TrackState::Cached]);
        let state = KeepState::of(&[d], &album("a"));
        assert_eq!(state, KeepState::Kept);
        assert_eq!(state.label(), "Kept");
    }

    #[test]
    fn a_failed_track_with_nothing_waiting_says_so_rather_than_kept() {
        let d = pinned("a", &[TrackState::Cached, TrackState::Failed]);
        assert_eq!(KeepState::of(&[d], &album("a")).label(), "1 track failed");
        let d = pinned("a", &[TrackState::Failed, TrackState::Failed]);
        assert_eq!(KeepState::of(&[d], &album("a")).label(), "2 tracks failed");
    }

    #[test]
    fn a_failed_track_while_others_wait_is_still_downloading() {
        let d = pinned("a", &[TrackState::Failed, TrackState::Queued]);
        let state = KeepState::of(&[d], &album("a"));
        assert_eq!(state, KeepState::Downloading { done: 0, total: 2 });
    }

    #[test]
    fn the_state_names_and_track_labels_are_distinct() {
        let mut names = vec![
            KeepState::NotKept.name(),
            KeepState::Downloading { done: 0, total: 1 }.name(),
            KeepState::Kept.name(),
            KeepState::Failed { failed: 1 }.name(),
        ];
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 4);
        let mut labels = vec![
            track_label(TrackState::Queued),
            track_label(TrackState::Cached),
            track_label(TrackState::Failed),
        ];
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), 3);
    }
}
