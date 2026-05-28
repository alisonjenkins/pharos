//! Live TV view (T56). Parity for jellyfin-web's `/#/livetv.html` page.
//!
//! Renders a channel grid with logo + name + number, plus the
//! current-window EPG strip per channel. Clicking a channel emits a
//! `Tune` action — caller routes to PlayerView pointing at
//! `/LiveTv/Channels/{id}/Stream`. The component is pure: caller
//! owns the fetch + state lifecycle.

use crate::client::{LiveChannel, LiveProgram};
use dioxus::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveTvAction {
    /// User clicked a channel tile — caller composes the stream URL.
    Tune { channel_id: String },
    /// User clicked the reload button.
    Refresh,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveTvStatus {
    Idle,
    Loading,
    Empty,
    Error(String),
}

#[component]
pub fn LiveTvView(
    channels: Vec<LiveChannel>,
    programs: Vec<LiveProgram>,
    status: LiveTvStatus,
    /// URL template with a literal `{id}` placeholder. When set,
    /// the component substitutes each channel's id and renders an
    /// `<img>`. None hides logos. Caller controls server base + auth
    /// query string — keeps the component renderer-free.
    logo_url_template: Option<String>,
    on_action: EventHandler<LiveTvAction>,
) -> Element {
    rsx! {
        section {
            class: "pharos-livetv",
            header {
                class: "pharos-livetv-header",
                h2 { "Live TV" }
                button {
                    class: "pharos-livetv-refresh",
                    onclick: move |_| on_action.call(LiveTvAction::Refresh),
                    "Refresh"
                }
            }
            match status {
                LiveTvStatus::Loading => rsx! {
                    p { class: "pharos-livetv-loading", "Loading channels…" }
                },
                LiveTvStatus::Error(e) => rsx! {
                    p { class: "pharos-error", "Live TV error: {e}" }
                },
                LiveTvStatus::Empty => rsx! {
                    p { class: "pharos-empty pharos-livetv-empty", "No channels configured" }
                },
                LiveTvStatus::Idle => render_grid(channels, programs, logo_url_template, on_action),
            }
        }
    }
}

fn render_grid(
    channels: Vec<LiveChannel>,
    programs: Vec<LiveProgram>,
    logo_url_template: Option<String>,
    on_action: EventHandler<LiveTvAction>,
) -> Element {
    rsx! {
        ul {
            class: "pharos-livetv-channels",
            for ch in channels.iter() {
                ChannelRow {
                    key: "{ch.id}",
                    channel: ch.clone(),
                    programs: programs
                        .iter()
                        .filter(|p| p.channel_id == ch.id)
                        .cloned()
                        .collect::<Vec<LiveProgram>>(),
                    logo_url_template: logo_url_template.clone(),
                    on_action: on_action,
                }
            }
        }
    }
}

#[component]
fn ChannelRow(
    channel: LiveChannel,
    programs: Vec<LiveProgram>,
    logo_url_template: Option<String>,
    on_action: EventHandler<LiveTvAction>,
) -> Element {
    let logo = if channel.has_logo {
        logo_url_template
            .as_ref()
            .map(|t| t.replace("{id}", &channel.id))
    } else {
        None
    };
    let cid = channel.id.clone();
    rsx! {
        li {
            class: "pharos-livetv-channel",
            "data-channel-id": "{channel.id}",
            button {
                class: "pharos-livetv-channel-tile",
                onclick: move |_| on_action.call(LiveTvAction::Tune { channel_id: cid.clone() }),
                if let Some(url) = logo.as_ref() {
                    img {
                        class: "pharos-livetv-channel-logo",
                        src: "{url}",
                        alt: "{channel.name}",
                    }
                }
                span { class: "pharos-livetv-channel-number", "{channel.number}" }
                span { class: "pharos-livetv-channel-name", "{channel.name}" }
            }
            ul {
                class: "pharos-livetv-epg",
                if programs.is_empty() {
                    li {
                        class: "pharos-livetv-epg-empty",
                        "no listings"
                    }
                } else {
                    for p in programs.iter() {
                        li {
                            class: "pharos-livetv-program",
                            key: "{p.id}",
                            span { class: "pharos-livetv-program-time", "{format_time(&p.start_iso)}" }
                            " "
                            span { class: "pharos-livetv-program-title", "{p.title}" }
                        }
                    }
                }
            }
        }
    }
}

/// Pull `HH:MM` from an ISO-8601 string. Cheap; covers our server
/// emission shape (`YYYY-MM-DDTHH:MM:SS.000Z`). Returns the raw input
/// on shape mismatch — better than throwing in a render.
fn format_time(iso: &str) -> String {
    if iso.len() >= 16 {
        iso[11..16].to_string()
    } else {
        iso.to_string()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn format_time_extracts_hh_mm() {
        assert_eq!(format_time("2026-05-28T10:30:00.000Z"), "10:30");
        assert_eq!(format_time("bad"), "bad");
    }

    #[test]
    fn live_tv_action_value_semantics() {
        let a = LiveTvAction::Tune {
            channel_id: "c1".into(),
        };
        assert_eq!(a, a.clone());
        assert_ne!(a, LiveTvAction::Refresh);
    }
}
