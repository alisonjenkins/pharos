//! Item detail view (T54). Parity surface for jellyfin-web's
//! `/#/details?id=…` page. Renders one item: title, kind, runtime,
//! Played + Favourite toggles, Play button, Resume tile when
//! playback_position_ticks > 0. Fetch + mutation live in the
//! WASM-side client; this component is pure (props in / events out).

use crate::api_types::ItemKind;
use crate::client::ItemDetail;
use dioxus::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetailAction {
    Play,
    Back,
    TogglePlayed,
    ToggleFavorite,
}

const TICKS_PER_SECOND: u64 = 10_000_000;

fn format_runtime(ticks: u64) -> String {
    if ticks == 0 {
        return "unknown".into();
    }
    let secs = ticks / TICKS_PER_SECOND;
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// "S01E03" — both indices known. Falls back to whichever fragment
/// is available, returning None when neither is set.
fn format_episode_index(season: Option<u32>, episode: Option<u32>) -> Option<String> {
    match (season, episode) {
        (Some(s), Some(e)) => Some(format!("S{s:02}E{e:02}")),
        (Some(s), None) => Some(format!("S{s:02}")),
        (None, Some(e)) => Some(format!("E{e:02}")),
        (None, None) => None,
    }
}

#[component]
pub fn ItemDetailView(
    detail: ItemDetail,
    error: Option<String>,
    /// When set, renders a `<img class="pharos-detail-primary">` tag
    /// pointing at the resolved image URL. None hides the figure.
    /// Caller composes the URL (`/Items/{id}/Images/Primary?api_key=…`).
    primary_image_url: Option<String>,
    on_action: EventHandler<DetailAction>,
) -> Element {
    let kind_label = detail.kind.label();
    let runtime = format_runtime(detail.run_time_ticks);
    let resumable = detail.playback_position_ticks > 0 && !detail.played;
    let resume_text = format_runtime(detail.playback_position_ticks);
    let episode_label = format_episode_index(detail.season_index, detail.episode_index);
    let series_label = detail.series_name.clone();
    let artists_line = if !detail.artists.is_empty() {
        Some(detail.artists.join(", "))
    } else {
        None
    };
    let album_line = detail.album.clone();
    let album_artists_line =
        if !detail.album_artists.is_empty() && detail.album_artists != detail.artists {
            Some(detail.album_artists.join(", "))
        } else {
            None
        };

    rsx! {
        article {
            class: "pharos-detail",
            "data-kind": "{kind_label}",
            header {
                class: "pharos-detail-header",
                button {
                    class: "pharos-detail-back",
                    onclick: move |_| on_action.call(DetailAction::Back),
                    "← Back"
                }
                h1 { class: "pharos-detail-title", "{detail.name}" }
                // Series breadcrumb above the title for episodes — mirrors
                // jellyfin-web's `series · S01E03` line. Only renders when
                // we actually have something to show.
                if detail.kind == ItemKind::Episode {
                    p {
                        class: "pharos-detail-series",
                        if let Some(s) = series_label.as_ref() {
                            span { class: "pharos-detail-series-name", "{s}" }
                        }
                        if let Some(ep) = episode_label.as_ref() {
                            if series_label.is_some() { " · " }
                            span { class: "pharos-detail-episode-index", "{ep}" }
                        }
                    }
                }
                if detail.kind == ItemKind::Audio {
                    p {
                        class: "pharos-detail-audio-meta",
                        if let Some(a) = artists_line.as_ref() {
                            span { class: "pharos-detail-artists", "{a}" }
                        }
                        if let Some(al) = album_line.as_ref() {
                            if artists_line.is_some() { " — " }
                            span { class: "pharos-detail-album", "{al}" }
                        }
                        if let Some(aa) = album_artists_line.as_ref() {
                            br {}
                            span {
                                class: "pharos-detail-album-artists",
                                "Album artist: {aa}"
                            }
                        }
                    }
                }
            }

            if let Some(url) = primary_image_url.as_ref() {
                figure {
                    class: "pharos-detail-primary",
                    img {
                        src: "{url}",
                        alt: "{detail.name}",
                    }
                }
            }

            if let Some(err) = error.as_ref() {
                p { class: "pharos-error", "{err}" }
            }

            dl {
                class: "pharos-detail-meta",
                dt { "Kind" } dd { "{kind_label}" }
                dt { "Runtime" } dd { class: "pharos-detail-runtime", "{runtime}" }
                dt { "Play count" } dd { class: "pharos-detail-playcount", "{detail.play_count}" }
            }

            div {
                class: "pharos-detail-actions",
                button {
                    class: "pharos-detail-play",
                    onclick: move |_| on_action.call(DetailAction::Play),
                    if resumable { "Resume" } else { "Play" }
                }
                if resumable {
                    span {
                        class: "pharos-detail-resume",
                        "Resume from {resume_text}"
                    }
                }
                button {
                    class: if detail.played { "pharos-detail-played on" } else { "pharos-detail-played off" },
                    onclick: move |_| on_action.call(DetailAction::TogglePlayed),
                    if detail.played { "Mark unplayed" } else { "Mark played" }
                }
                button {
                    class: if detail.is_favorite { "pharos-detail-favorite on" } else { "pharos-detail-favorite off" },
                    onclick: move |_| on_action.call(DetailAction::ToggleFavorite),
                    if detail.is_favorite { "★ Favourite" } else { "☆ Favourite" }
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn format_runtime_branches() {
        assert_eq!(format_runtime(0), "unknown");
        assert_eq!(format_runtime(10 * TICKS_PER_SECOND), "10s");
        assert_eq!(format_runtime(125 * TICKS_PER_SECOND), "2m 05s");
        assert_eq!(format_runtime(3725 * TICKS_PER_SECOND), "1h 02m");
    }

    #[test]
    fn detail_action_value_semantics() {
        let a = DetailAction::Play;
        assert_eq!(a, DetailAction::Play);
        assert_ne!(DetailAction::Play, DetailAction::Back);
    }

    #[test]
    fn format_episode_index_branches() {
        assert_eq!(
            format_episode_index(Some(1), Some(3)).as_deref(),
            Some("S01E03")
        );
        assert_eq!(format_episode_index(Some(2), None).as_deref(), Some("S02"));
        assert_eq!(format_episode_index(None, Some(7)).as_deref(), Some("E07"));
        assert_eq!(format_episode_index(None, None), None);
    }
}
