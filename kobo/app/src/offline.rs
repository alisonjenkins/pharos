//! What the app keeps in its own small store so it works with the server out
//! of reach: the albums that have downloads, with their tracks, and the places
//! the listener reached that the server has not heard yet.
//!
//! The store holds at most [`kobo_sdk::MAX_STORE_VALUE`] bytes in a value, far
//! too little for a library and plenty for the names of a few hundred tracks.
//! Both values are rewritten whole, oldest dropped first when one is full.

use crate::api::{Entry, Page};
use std::collections::HashMap;

/// The most the saved albums may take. Under the store's own cap so a value
/// that fits here is never refused there.
const OFFLINE_BYTES: usize = 192 * 1024;
/// The most unsent places kept; the oldest is dropped for a new one.
const MAX_UNSENT: usize = 32;

fn clean(text: &str) -> String {
    text.chars()
        .map(|c| {
            if matches!(c, '\t' | '\n' | '\r') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

fn line(kind: char, entry: &Entry) -> String {
    let runtime = entry
        .runtime_ms
        .map_or_else(|| "-".to_owned(), |ms| ms.to_string());
    format!(
        "{kind}\t{}\t{}\t{}\t{runtime}\n",
        clean(&entry.id),
        clean(&entry.name),
        clean(&entry.by)
    )
}

fn entry_of(fields: &[&str]) -> Option<Entry> {
    let [id, name, by, runtime] = fields else {
        return None;
    };
    (!id.is_empty()).then(|| Entry {
        id: (*id).to_owned(),
        name: (*name).to_owned(),
        by: (*by).to_owned(),
        runtime_ms: runtime.parse().ok(),
    })
}

/// The albums with something downloaded, and their tracks, oldest first.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Offline {
    albums: Vec<(Entry, Vec<Entry>)>,
}

impl Offline {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.albums.is_empty()
    }

    #[must_use]
    pub fn contains(&self, album: &str) -> bool {
        self.albums.iter().any(|(entry, _)| entry.id == album)
    }

    /// Records `album` with its `tracks` as the newest. `true` when that
    /// changed anything, so an unchanged album is not written again.
    pub fn keep(&mut self, album: &Entry, tracks: &[Entry]) -> bool {
        if self
            .albums
            .last()
            .is_some_and(|(last, kept)| last == album && kept == tracks)
        {
            return false;
        }
        self.albums.retain(|(entry, _)| entry.id != album.id);
        self.albums.push((album.clone(), tracks.to_vec()));
        true
    }

    /// Forgets `album`. `true` when it was there.
    pub fn forget(&mut self, album: &str) -> bool {
        let before = self.albums.len();
        self.albums.retain(|(entry, _)| entry.id != album);
        self.albums.len() != before
    }

    /// The saved albums as a listing, by name.
    #[must_use]
    pub fn page(&self) -> Page {
        let mut entries: Vec<Entry> = self.albums.iter().map(|(entry, _)| entry.clone()).collect();
        entries.sort_by_key(|entry| entry.name.to_lowercase());
        Page {
            total: entries.len(),
            entries,
        }
    }

    #[must_use]
    pub fn tracks(&self, album: &str) -> Option<Page> {
        self.albums
            .iter()
            .find(|(entry, _)| entry.id == album)
            .map(|(_, tracks)| Page {
                entries: tracks.clone(),
                total: tracks.len(),
            })
    }

    /// One line per album and per track, tab separated. Albums are dropped
    /// oldest first until the value fits.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut kept: Vec<String> = self
            .albums
            .iter()
            .map(|(album, tracks)| {
                let mut text = line('A', album);
                for track in tracks {
                    text.push_str(&line('T', track));
                }
                text
            })
            .collect();
        while kept.iter().map(String::len).sum::<usize>() > OFFLINE_BYTES && !kept.is_empty() {
            kept.remove(0);
        }
        kept.concat().into_bytes()
    }

    /// Lines that do not read are skipped, so one damaged line costs one
    /// track and not the library.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Self {
        let mut albums: Vec<(Entry, Vec<Entry>)> = Vec::new();
        for text in String::from_utf8_lossy(bytes).lines() {
            let fields: Vec<&str> = text.split('\t').collect();
            let Some((kind, rest)) = fields.split_first() else {
                continue;
            };
            let Some(entry) = entry_of(rest) else {
                continue;
            };
            match *kind {
                "A" => albums.push((entry, Vec::new())),
                "T" => {
                    if let Some((_, tracks)) = albums.last_mut() {
                        tracks.push(entry);
                    }
                }
                _ => {}
            }
        }
        Self { albums }
    }
}

/// A place the listener reached in a track, with the order it was reported in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Place {
    pub track: String,
    pub position_ms: u64,
    /// When the report that carried this place was issued: later is larger,
    /// whatever the positions say (a seek back is newer and smaller). Places
    /// read from the store are 0, older than anything this run issues.
    pub issued: u64,
}

/// Places the listener reached that the server has not been told, by track.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Unsent {
    places: Vec<Place>,
    /// The newest report of each track the server has acknowledged. Anything
    /// issued before it is stale, so a late failure does not bring it back.
    heard: HashMap<String, u64>,
}

impl Unsent {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.places.is_empty()
    }

    #[must_use]
    pub fn places(&self) -> &[Place] {
        &self.places
    }

    /// Notes the place reached in `track` by the report `issued`, replacing
    /// an older one. A place older than one already kept, or than what the
    /// server has heard, is dropped. `true` when the kept places changed.
    pub fn note(&mut self, track: &str, position_ms: u64, issued: u64) -> bool {
        if self.heard.get(track).is_some_and(|heard| *heard >= issued) {
            return false;
        }
        if self
            .places
            .iter()
            .any(|place| place.track == track && place.issued > issued)
        {
            return false;
        }
        self.places.retain(|place| place.track != track);
        self.places.push(Place {
            track: clean(track),
            position_ms,
            issued,
        });
        if self.places.len() > MAX_UNSENT {
            self.places.remove(0);
        }
        true
    }

    /// The server heard the report `issued` for `track`: forgets the kept
    /// place if it is that one or older. A place kept from a later report
    /// stays. `true` when a place was forgotten.
    pub fn sent(&mut self, track: &str, issued: u64) -> bool {
        let heard = self.heard.entry(track.to_owned()).or_insert(issued);
        *heard = (*heard).max(issued);
        let before = self.places.len();
        self.places
            .retain(|place| place.track != track || place.issued > issued);
        self.places.len() != before
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut text = String::new();
        for place in &self.places {
            text.push_str(&format!("{}\t{}\n", place.track, place.position_ms));
        }
        text.into_bytes()
    }

    #[must_use]
    pub fn decode(bytes: &[u8]) -> Self {
        let places = String::from_utf8_lossy(bytes)
            .lines()
            .filter_map(|text| {
                let (track, ms) = text.split_once('\t')?;
                let position_ms = ms.parse().ok()?;
                (!track.is_empty()).then(|| Place {
                    track: track.to_owned(),
                    position_ms,
                    issued: 0,
                })
            })
            .take(MAX_UNSENT)
            .collect();
        Self {
            places,
            heard: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, name: &str) -> Entry {
        Entry {
            id: id.to_owned(),
            name: name.to_owned(),
            by: "Band".to_owned(),
            runtime_ms: Some(1_000),
        }
    }

    #[test]
    fn saved_albums_round_trip_with_their_tracks_and_survive_a_damaged_line() {
        let mut offline = Offline::default();
        assert!(offline.keep(
            &entry("al1", "First\tAlbum"),
            &[entry("t1", "One"), entry("t2", "Two")]
        ));
        assert!(offline.keep(&entry("al2", "Second"), &[entry("t3", "Three")]));
        let bytes = offline.encode();
        let back = Offline::decode(&bytes);
        assert_eq!(back.page().entries.len(), 2);
        assert_eq!(back.tracks("al1").expect("tracks").entries.len(), 2);
        assert_eq!(back.tracks("al1").expect("tracks").entries[0].name, "One");
        let mut damaged = b"T\tjunk\n".to_vec();
        damaged.extend_from_slice(&bytes);
        assert_eq!(Offline::decode(&damaged), back);
        assert!(Offline::decode(b"\xff\xfe not text").is_empty());
    }

    #[test]
    fn keeping_the_same_album_again_changes_nothing_and_forgetting_removes_it() {
        let mut offline = Offline::default();
        let tracks = [entry("t1", "One")];
        assert!(offline.keep(&entry("al1", "First"), &tracks));
        assert!(!offline.keep(&entry("al1", "First"), &tracks));
        assert!(offline.contains("al1"));
        assert!(offline.forget("al1"));
        assert!(!offline.forget("al1"));
        assert!(!offline.contains("al1"));
    }

    #[test]
    fn a_full_library_drops_its_oldest_album_first() {
        let mut offline = Offline::default();
        let long = "x".repeat(200);
        for at in 0..2_000 {
            let tracks: Vec<Entry> = (0..3)
                .map(|t| entry(&format!("t{at}-{t}"), &long))
                .collect();
            offline.keep(&entry(&format!("al{at}"), &long), &tracks);
        }
        let bytes = offline.encode();
        assert!(bytes.len() <= OFFLINE_BYTES);
        assert!(bytes.len() < kobo_sdk::MAX_STORE_VALUE);
        let back = Offline::decode(&bytes);
        assert!(back.contains("al1999"));
        assert!(!back.contains("al0"));
    }

    fn kept(unsent: &Unsent) -> Vec<(&str, u64)> {
        unsent
            .places()
            .iter()
            .map(|place| (place.track.as_str(), place.position_ms))
            .collect()
    }

    #[test]
    fn an_unsent_place_is_replaced_by_a_newer_one_and_the_oldest_is_dropped_when_full() {
        let mut unsent = Unsent::default();
        assert!(unsent.note("t1", 5, 1));
        assert!(unsent.note("t1", 9, 2));
        assert_eq!(kept(&unsent), [("t1", 9)]);
        for at in 0..40 {
            unsent.note(&format!("x{at}"), at, 3 + at);
        }
        assert_eq!(unsent.places().len(), MAX_UNSENT);
        let back = Unsent::decode(&unsent.encode());
        assert_eq!(kept(&back), kept(&unsent));
        assert!(unsent.sent("x39", 42));
        assert!(!unsent.sent("x39", 42));
    }

    #[test]
    fn the_later_report_wins_whatever_the_order_of_arrival_or_the_positions() {
        // A seek back: report 2 is newer than report 1 and smaller.
        let mut unsent = Unsent::default();
        assert!(unsent.note("t1", 100, 2));
        assert!(!unsent.note("t1", 900, 1));
        assert_eq!(kept(&unsent), [("t1", 100)]);
        let mut unsent = Unsent::default();
        assert!(unsent.note("t1", 900, 1));
        assert!(unsent.note("t1", 100, 2));
        assert_eq!(kept(&unsent), [("t1", 100)]);
    }

    #[test]
    fn a_success_forgets_only_what_it_carried_or_older() {
        let mut unsent = Unsent::default();
        unsent.note("t1", 100, 2);
        assert!(!unsent.sent("t1", 1), "an older report heard");
        assert_eq!(kept(&unsent), [("t1", 100)]);
        assert!(unsent.sent("t1", 2));
        assert!(unsent.is_empty());
    }

    #[test]
    fn a_report_the_server_heard_is_not_kept_by_an_older_failure() {
        let mut unsent = Unsent::default();
        assert!(!unsent.sent("t1", 2));
        assert!(!unsent.note("t1", 900, 1));
        assert!(unsent.is_empty());
        assert!(unsent.note("t1", 5, 3));
    }
}
