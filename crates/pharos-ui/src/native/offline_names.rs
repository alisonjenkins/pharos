//! The names of the albums kept for offline (spec 044): what the Downloads
//! list and the offline start screen call each one when the host's saved
//! listings do not name it. The host stores only ids; the title is known to
//! the page that pins, so that page notes it here, on the device, beside the
//! cache. Nothing in this record is read from the network.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::platform;
use super::ports::{ContainerRef, Download};

/// Where the record is, in the device-local preference store.
pub const STORE_KEY: &str = "pharos.offline.names";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Name {
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subtitle: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Names(BTreeMap<String, Name>);

fn key(container: &ContainerRef) -> String {
    format!("{}/{}", container.kind.label(), container.id)
}

impl Names {
    /// The record as stored; a missing or unreadable one is empty.
    pub fn parse(stored: Option<&str>) -> Self {
        stored
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_owned())
    }

    /// Notes the name of a container being kept. A blank title is not noted.
    pub fn remember(&mut self, container: &ContainerRef, title: &str, subtitle: Option<&str>) {
        let title = title.trim();
        if title.is_empty() {
            return;
        }
        self.0.insert(
            key(container),
            Name {
                title: title.to_owned(),
                subtitle: subtitle
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
            },
        );
    }

    pub fn forget(&mut self, container: &ContainerRef) -> bool {
        self.0.remove(&key(container)).is_some()
    }

    /// Fills the title and subtitle the host left blank. What the host named
    /// wins; a container this record does not know stays as it was.
    pub fn fill(&self, downloads: Vec<Download>) -> Vec<Download> {
        downloads
            .into_iter()
            .map(|mut d| {
                if let Some(n) = self.0.get(&key(&d.container)) {
                    if d.title.trim().is_empty() {
                        d.title = n.title.clone();
                    }
                    if d.subtitle.is_none() {
                        d.subtitle = n.subtitle.clone();
                    }
                }
                d
            })
            .collect()
    }

    pub fn load() -> Self {
        match platform::stored_theme(STORE_KEY) {
            Ok(stored) => Self::parse(stored.as_deref()),
            Err(e) => {
                platform::warn(&format!("pharos: offline names unreadable error={e}"));
                Self::default()
            }
        }
    }

    pub fn save(&self) {
        if let Err(e) = platform::store_theme(STORE_KEY, &self.to_json()) {
            platform::warn(&format!("pharos: offline names not kept error={e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::ports::{ContainerKind, TrackDownload, TrackState};

    fn album(id: &str) -> ContainerRef {
        ContainerRef {
            kind: ContainerKind::Album,
            id: id.into(),
        }
    }

    fn download(id: &str, title: &str, subtitle: Option<&str>) -> Download {
        Download {
            container: album(id),
            title: title.into(),
            subtitle: subtitle.map(str::to_owned),
            tracks: vec![TrackDownload {
                id: "t".into(),
                title: "Track one".into(),
                state: TrackState::Cached,
                bytes: 1,
            }],
        }
    }

    #[test]
    fn a_remembered_name_survives_the_stored_form() {
        let mut names = Names::default();
        names.remember(&album("1"), " Book one ", Some("Author one"));
        let back = Names::parse(Some(&names.to_json()));
        assert_eq!(back, names);
    }

    #[test]
    fn a_missing_or_broken_record_is_empty() {
        assert_eq!(Names::parse(None), Names::default());
        assert_eq!(Names::parse(Some("not json")), Names::default());
    }

    #[test]
    fn a_blank_title_is_not_remembered() {
        let mut names = Names::default();
        names.remember(&album("1"), "  ", Some("Author one"));
        assert_eq!(names, Names::default());
    }

    #[test]
    fn fill_names_what_the_host_left_blank_and_keeps_what_it_named() {
        let mut names = Names::default();
        names.remember(&album("1"), "Book one", Some("Author one"));
        names.remember(&album("2"), "Book two", None);
        let filled = names.fill(vec![
            download("1", "", None),
            download("2", "Host title", Some("Host author")),
            download("3", "", None),
        ]);
        assert_eq!(filled[0].title, "Book one");
        assert_eq!(filled[0].subtitle.as_deref(), Some("Author one"));
        assert_eq!(filled[1].title, "Host title");
        assert_eq!(filled[1].subtitle.as_deref(), Some("Host author"));
        assert_eq!(filled[2].title, "");
    }

    #[test]
    fn a_forgotten_name_no_longer_fills() {
        let mut names = Names::default();
        names.remember(&album("1"), "Book one", None);
        assert!(names.forget(&album("1")));
        assert!(!names.forget(&album("1")));
        assert_eq!(names.fill(vec![download("1", "", None)])[0].title, "");
    }

    #[test]
    fn a_mix_and_an_album_with_the_same_id_do_not_share_a_name() {
        let mix = ContainerRef {
            kind: ContainerKind::Mix,
            id: "1".into(),
        };
        let mut names = Names::default();
        names.remember(&mix, "Mix one", None);
        assert_eq!(names.fill(vec![download("1", "", None)])[0].title, "");
    }
}
