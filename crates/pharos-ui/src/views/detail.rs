//! Item detail view (T54). Parity surface for jellyfin-web's
//! `/#/details?id=…` page. Renders one item: title, kind, runtime,
//! Played + Favourite toggles, Play button, Resume tile when
//! playback_position_ticks > 0. Fetch + mutation live in the
//! WASM-side client; this component is pure (props in / events out).

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

#[component]
pub fn ItemDetailView(
    detail: ItemDetail,
    error: Option<String>,
    on_action: EventHandler<DetailAction>,
) -> Element {
    let kind_label = detail.kind.label();
    let runtime = format_runtime(detail.run_time_ticks);
    let resumable = detail.playback_position_ticks > 0 && !detail.played;
    let resume_text = format_runtime(detail.playback_position_ticks);

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
}
