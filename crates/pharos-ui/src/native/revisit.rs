//! What Home and title pages last showed, and where the viewer had
//! scrolled, so Back draws them at once while they refresh (FR-206, A11).
//! Bounded: one Home, [`TITLES`] titles and [`PLACES`] scroll positions.

use std::collections::VecDeque;

use dioxus::prelude::*;
use pharos_client::api::{Detail, Home, ItemRef};

/// Title pages kept for Back.
pub const TITLES: usize = 4;
/// Scroll positions kept, one per page path.
pub const PLACES: usize = 16;

/// The `cap` most recently put values by key, newest last.
#[derive(Debug, Clone, PartialEq)]
pub struct Recent<K, V> {
    cap: usize,
    entries: VecDeque<(K, V)>,
}

impl<K: PartialEq, V> Recent<K, V> {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            entries: VecDeque::new(),
        }
    }

    /// Keeps `value` under `key`, replacing any earlier one and dropping the
    /// oldest past the cap.
    pub fn put(&mut self, key: K, value: V) {
        self.entries.retain(|(k, _)| *k != key);
        self.entries.push_back((key, value));
        while self.entries.len() > self.cap {
            self.entries.pop_front();
        }
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The app's memory of pages for Back. `back` is whether the page now
/// shown was reached by Back or Forward rather than a link.
#[derive(Clone, Copy)]
pub struct Revisit {
    pub home: Signal<Option<Home>>,
    pub titles: Signal<Recent<ItemRef, Detail>>,
    pub places: Signal<Recent<String, f64>>,
    pub back: Signal<bool>,
}

impl Revisit {
    pub fn new() -> Self {
        Self {
            home: Signal::new(None),
            titles: Signal::new(Recent::new(TITLES)),
            places: Signal::new(Recent::new(PLACES)),
            back: Signal::new(false),
        }
    }

    /// The scroll position to put back on the page at `path`: only on a
    /// return by Back or Forward, and only when one was kept.
    pub fn place_for(&self, path: &str) -> Option<f64> {
        if !*self.back.peek() {
            return None;
        }
        self.places.peek().get(&path.to_owned()).copied()
    }
}

impl Default for Revisit {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn recent_keeps_the_newest_value_per_key_up_to_its_cap() {
        let mut r = Recent::new(2);
        r.put("a", 1);
        r.put("b", 2);
        r.put("a", 3);
        assert_eq!(r.get(&"a"), Some(&3));
        assert_eq!(r.len(), 2);
        r.put("c", 4);
        assert_eq!(r.get(&"b"), None, "the oldest goes first");
        assert_eq!(r.get(&"a"), Some(&3));
        assert_eq!(r.get(&"c"), Some(&4));
    }

    #[test]
    fn a_place_is_put_back_only_on_a_return() {
        let mut dom = VirtualDom::new(|| {
            let revisit = use_hook(Revisit::new);
            let mut places = revisit.places;
            places.write().put("/ui/".to_owned(), 640.0);
            assert_eq!(revisit.place_for("/ui/"), None, "a link starts at the top");
            let mut back = revisit.back;
            back.set(true);
            assert_eq!(revisit.place_for("/ui/"), Some(640.0));
            assert_eq!(revisit.place_for("/ui/title/r1"), None);
            rsx! {}
        });
        dom.rebuild_in_place();
    }
}
